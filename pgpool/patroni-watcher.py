#!/usr/bin/env python3
"""
Patroni Watcher - Main process that manages Pgpool and syncs with Patroni cluster.
Spawns pgpool, monitors its health, and constantly reconciles backend state.
If this process crashes, the container exits.
"""

import os
import sys
import time
import signal
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
PGPOOL_CONF_DIR = os.environ.get("PGPOOL_CONF_DIR", "/opt/bitnami/pgpool/conf")
PGPOOL_BIN_DIR = os.environ.get("PGPOOL_BIN_DIR", "/opt/bitnami/pgpool/bin")
POLL_INTERVAL = 2
PATRONI_TIMEOUT = 3

# Global pgpool process
pgpool_process = None


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
            name = host.split(".")[0]
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
        data = response.json()
        role = data.get("role")
        if role:
            return role
    except requests.exceptions.Timeout:
        logger.debug(f"Timeout reaching Patroni at {host}:8008")
    except requests.exceptions.ConnectionError:
        logger.debug(f"Connection error to {host}:8008")
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


def reconcile_pgpool(backends, patroni_state):
    """
    Reconcile pgpool backend status with Patroni state.
    Always checks and corrects state, not just on changes.
    """
    changes_made = False

    for backend in backends:
        idx = backend["index"]
        patroni_role = patroni_state.get(idx)
        pgpool_status, pgpool_role = get_pgpool_node_status(idx)

        # Node is healthy in Patroni
        if patroni_role in ("primary", "standby"):
            # Attach if not up (status != 1)
            if pgpool_status != 1:
                logger.info(f"Attaching {backend['name']} (patroni: {patroni_role}, pgpool_status: {pgpool_status})")
                if run_pcp_command("pcp_attach_node", "-n", str(idx)):
                    changes_made = True
        else:
            # Node is unhealthy in Patroni - detach if up
            if pgpool_status == 1:
                logger.info(f"Detaching {backend['name']} (patroni: {patroni_role})")
                if run_pcp_command("pcp_detach_node", "-n", str(idx)):
                    changes_made = True

    return changes_made


def get_leader_name(patroni_state, backends):
    """Get the name of the current leader"""
    for backend in backends:
        if patroni_state.get(backend["index"]) == "primary":
            return backend["name"]
    return None


def start_pgpool():
    """Start pgpool as a subprocess"""
    global pgpool_process

    pgpool_conf = f"{PGPOOL_CONF_DIR}/pgpool.conf"
    pcp_conf = f"{PGPOOL_CONF_DIR}/pcp.conf"
    pgpool_bin = f"{PGPOOL_BIN_DIR}/pgpool"

    logger.info("Starting pgpool...")
    pgpool_process = subprocess.Popen(
        [pgpool_bin, "-n", "-f", pgpool_conf, "-F", pcp_conf],
        stdout=sys.stdout,
        stderr=sys.stderr
    )
    logger.info(f"Pgpool started with PID {pgpool_process.pid}")
    return pgpool_process


def check_pgpool_health():
    """Check if pgpool process is still running"""
    global pgpool_process
    if pgpool_process is None:
        return False

    poll = pgpool_process.poll()
    if poll is not None:
        logger.error(f"Pgpool process exited with code {poll}")
        return False
    return True


def wait_for_pcp_ready(timeout=60):
    """Wait for pgpool PCP port to be ready"""
    import socket

    logger.info("Waiting for pgpool PCP port...")
    start = time.time()
    while time.time() - start < timeout:
        try:
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
                s.settimeout(2)
                if s.connect_ex(("localhost", 9898)) == 0:
                    logger.info("Pgpool PCP ready")
                    return True
        except:
            pass
        time.sleep(1)

    logger.error("Timeout waiting for pgpool PCP")
    return False


def shutdown(signum, frame):
    """Handle shutdown signals"""
    global pgpool_process
    logger.info(f"Received signal {signum}, shutting down...")

    if pgpool_process:
        logger.info("Stopping pgpool...")
        pgpool_process.terminate()
        try:
            pgpool_process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            logger.warning("Pgpool didn't stop gracefully, killing...")
            pgpool_process.kill()

    sys.exit(0)


def main():
    global pgpool_process

    if not PCP_PASSWORD:
        logger.error("PGPOOL_ADMIN_PASSWORD not set")
        sys.exit(1)

    # Setup signal handlers
    signal.signal(signal.SIGTERM, shutdown)
    signal.signal(signal.SIGINT, shutdown)

    backends = parse_backend_nodes()
    logger.info(f"Monitoring backends: {[b['name'] for b in backends]}")

    # Start pgpool
    start_pgpool()

    # Wait for PCP to be ready
    if not wait_for_pcp_ready():
        logger.error("Failed to start pgpool")
        if pgpool_process:
            pgpool_process.kill()
        sys.exit(1)

    last_leader = None
    reconcile_count = 0

    while True:
        try:
            # Check pgpool health first
            if not check_pgpool_health():
                logger.error("Pgpool died, exiting...")
                sys.exit(1)

            # Get current Patroni state
            patroni_state = get_cluster_state(backends)
            leader_name = get_leader_name(patroni_state, backends)

            # Log leader changes
            if leader_name != last_leader:
                if leader_name:
                    logger.info(f"Leader: {leader_name}")
                else:
                    logger.warning("No leader found in Patroni cluster")
                last_leader = leader_name

            # Always reconcile - don't skip even if state looks the same
            reconcile_pgpool(backends, patroni_state)

            # Periodic status log
            reconcile_count += 1
            if reconcile_count % 30 == 0:  # Every ~60 seconds
                healthy = sum(1 for r in patroni_state.values() if r is not None)
                logger.info(f"Status: {healthy}/{len(backends)} backends healthy, leader: {leader_name or 'none'}")

            time.sleep(POLL_INTERVAL)

        except KeyboardInterrupt:
            shutdown(signal.SIGINT, None)
        except Exception as e:
            logger.error(f"Error in main loop: {e}")
            # Don't exit on transient errors, but check pgpool is still alive
            if not check_pgpool_health():
                logger.error("Pgpool died during error recovery, exiting...")
                sys.exit(1)
            time.sleep(POLL_INTERVAL)


if __name__ == "__main__":
    main()
