#!/usr/bin/env python3
"""
Patroni Watcher - Monitors Patroni cluster and updates Pgpool backends
Polls Patroni REST API, uses PCP to attach/detach backends based on Patroni state
"""

import os
import sys
import time
import socket
import subprocess
import logging
import requests

logging.basicConfig(
    level=logging.INFO,
    format='[patroni-watcher] %(levelname)s: %(message)s',
    stream=sys.stderr
)
logger = logging.getLogger(__name__)

# Configuration
PCP_USER = os.environ.get("PGPOOL_ADMIN_USERNAME", "admin")
PCP_PASSWORD = os.environ.get("PGPOOL_ADMIN_PASSWORD", "")
POLL_INTERVAL = 2
PATRONI_TIMEOUT = 3


def parse_backend_nodes():
    """Parse PGPOOL_BACKEND_NODES env var: '0:host1:5432,1:host2:5432,...'"""
    nodes_str = os.environ.get("PGPOOL_BACKEND_NODES", "")
    if not nodes_str:
        logger.error("PGPOOL_BACKEND_NODES not set")
        sys.exit(1)

    backends = []
    for node in nodes_str.split(","):
        parts = node.strip().split(":")
        if len(parts) >= 3:
            index, host, port = int(parts[0]), parts[1], int(parts[2])
            name = host.split(".")[0]  # postgres-1.railway.internal -> postgres-1
            backends.append({"name": name, "host": host, "port": port, "index": index})

    return backends


def run_pcp_command(cmd, *args, retries=3, capture=False):
    """Execute PCP command with retry logic"""
    env = os.environ.copy()
    env["PCPPASSFILE"] = "/tmp/.pcppass"
    full_cmd = [cmd, "-h", "localhost", "-p", "9898", "-U", PCP_USER, "-w", *args]

    for attempt in range(retries):
        try:
            result = subprocess.run(full_cmd, env=env, capture_output=True, text=True, timeout=10)
            if result.returncode == 0:
                return result.stdout.strip() if capture else True
            if attempt < retries - 1:
                time.sleep(0.5)
        except subprocess.TimeoutExpired:
            logger.warning(f"PCP command timed out: {cmd}")
        except Exception as e:
            logger.warning(f"PCP error: {e}")

    return None if capture else False


def get_pgpool_node_status(node_index):
    """Get node status from pgpool: returns (status, role) or (None, None)
    Status: 0=init, 1=up, 2=down, 3=quarantine
    Role: 0=standby, 1=primary
    """
    output = run_pcp_command("pcp_node_info", "-n", str(node_index), capture=True)
    if output:
        # Output format: hostname port status lb_weight role ...
        parts = output.split()
        if len(parts) >= 5:
            try:
                return int(parts[2]), int(parts[4])
            except (ValueError, IndexError):
                pass
    return None, None


def get_patroni_role(host):
    """Query Patroni REST API for node role (master/replica)"""
    try:
        response = requests.get(f"http://{host}:8008/", timeout=PATRONI_TIMEOUT)
        # Patroni returns 200 for primary, 503 for replica (healthy but not primary)
        # Both are valid - check the JSON body for role
        data = response.json()
        role = data.get("role")
        if role:
            return role
        logger.debug(f"No role in response from {host}: {data}")
    except requests.exceptions.Timeout:
        logger.debug(f"Timeout reaching Patroni at {host}:8008")
    except requests.exceptions.ConnectionError as e:
        logger.debug(f"Connection error to {host}:8008: {e}")
    except Exception as e:
        logger.debug(f"Error querying {host}: {e}")
    return None


def get_cluster_state(backends):
    """Get cluster state from Patroni - returns dict of {index: role}"""
    state = {}
    for backend in backends:
        role = get_patroni_role(backend["host"])
        if role in ("master", "primary"):
            state[backend["index"]] = "primary"
        elif role in ("replica", "standby"):
            state[backend["index"]] = "standby"
        else:
            state[backend["index"]] = None
    return state


def sync_pgpool(backends, patroni_state):
    """Sync pgpool backend status with Patroni state, minimizing unnecessary operations"""
    for backend in backends:
        idx = backend["index"]
        patroni_role = patroni_state.get(idx)
        pgpool_status, pgpool_role = get_pgpool_node_status(idx)

        # Node is healthy in Patroni
        if patroni_role in ("primary", "standby"):
            # Only attach if not already up (status 1)
            if pgpool_status != 1:
                logger.info(f"Attaching {backend['name']} (patroni: {patroni_role})")
                run_pcp_command("pcp_attach_node", "-n", str(idx))
        else:
            # Node is unhealthy in Patroni - detach if up
            if pgpool_status == 1:
                logger.info(f"Detaching {backend['name']} (patroni: {patroni_role})")
                run_pcp_command("pcp_detach_node", "-n", str(idx))


def get_leader_name(patroni_state, backends):
    """Get the name of the current leader"""
    for backend in backends:
        if patroni_state.get(backend["index"]) == "primary":
            return backend["name"]
    return None


def wait_for_pgpool():
    """Wait for pgpool PCP port using socket"""
    logger.info("Waiting for pgpool...")
    for _ in range(60):
        try:
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
                s.settimeout(2)
                if s.connect_ex(("localhost", 9898)) == 0:
                    logger.info("Pgpool ready")
                    return True
        except:
            pass
        time.sleep(2)
    logger.error("Timeout waiting for pgpool")
    return False


def main():
    if not PCP_PASSWORD:
        logger.error("PGPOOL_ADMIN_PASSWORD not set")
        sys.exit(1)

    backends = parse_backend_nodes()
    logger.info(f"Monitoring: {[b['name'] for b in backends]}")

    if not wait_for_pgpool():
        sys.exit(1)

    last_state = {}

    while True:
        try:
            patroni_state = get_cluster_state(backends)
            leader_name = get_leader_name(patroni_state, backends)

            # Only sync if state actually changed
            if patroni_state != last_state:
                if leader_name:
                    logger.info(f"State change detected, leader: {leader_name}")
                else:
                    logger.warning("No leader found in Patroni")
                sync_pgpool(backends, patroni_state)
                last_state = patroni_state.copy()

            time.sleep(POLL_INTERVAL)
        except KeyboardInterrupt:
            break
        except Exception as e:
            logger.error(f"Error: {e}")
            time.sleep(POLL_INTERVAL)


if __name__ == "__main__":
    main()
