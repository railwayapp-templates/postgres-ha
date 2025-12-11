#!/usr/bin/env python3
"""
Patroni Watcher - Monitors Patroni cluster and updates Pgpool backends
Polls Patroni REST API, uses PCP to attach leader and detach replicas
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


def run_pcp_command(cmd, *args, retries=3):
    """Execute PCP command with retry logic"""
    env = os.environ.copy()
    env["PCPPASSFILE"] = "/tmp/.pcppass"
    full_cmd = [cmd, "-h", "localhost", "-p", "9898", "-U", PCP_USER, "-w", *args]

    for attempt in range(retries):
        try:
            result = subprocess.run(full_cmd, env=env, capture_output=True, text=True, timeout=10)
            if result.returncode == 0:
                return True
            if attempt < retries - 1:
                time.sleep(0.5)
        except subprocess.TimeoutExpired:
            logger.warning(f"PCP command timed out: {cmd}")
        except Exception as e:
            logger.warning(f"PCP error: {e}")

    return False


def get_patroni_role(host):
    """Query Patroni REST API for node role (master/replica)"""
    try:
        response = requests.get(f"http://{host}:8008/", timeout=PATRONI_TIMEOUT)
        if response.status_code == 200:
            return response.json().get("role")
    except:
        pass
    return None


def get_cluster_state(backends):
    """Get cluster state - returns (leader, healthy_backends)"""
    leader = None
    healthy = []
    for backend in backends:
        role = get_patroni_role(backend["host"])
        if role in ("master", "primary"):
            leader = backend
            healthy.append(backend)
        elif role in ("replica", "standby"):
            healthy.append(backend)
    return leader, healthy


def get_cluster_leader(backends):
    """Find current leader from backends"""
    leader, _ = get_cluster_state(backends)
    return leader


def sync_pgpool(leader, backends):
    """Attach all backends - pgpool determines primary/standby from postgres itself"""
    logger.info(f"Syncing: leader is {leader['name']}")
    for backend in backends:
        # Attach all backends, not just leader
        # Pgpool uses streaming replication check to determine roles
        run_pcp_command("pcp_attach_node", "-n", str(backend["index"]))


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

    last_leader = None
    last_healthy_count = 0
    sync_interval = 0  # Force initial sync

    while True:
        try:
            leader, healthy = get_cluster_state(backends)
            healthy_count = len(healthy)

            # Sync on leader change, healthy node count change, or every 30 seconds
            should_sync = (
                (leader and leader["name"] != last_leader) or
                (healthy_count != last_healthy_count) or
                (sync_interval >= 15)  # Every 30 seconds (15 * 2s poll interval)
            )

            if leader is None:
                logger.warning("No leader found")
            elif should_sync:
                if leader["name"] != last_leader:
                    logger.info(f"Leader change: {last_leader or 'none'} -> {leader['name']}")
                elif healthy_count != last_healthy_count:
                    logger.info(f"Healthy nodes changed: {last_healthy_count} -> {healthy_count}")
                sync_pgpool(leader, backends)
                last_leader = leader["name"]
                last_healthy_count = healthy_count
                sync_interval = 0

            sync_interval += 1
            time.sleep(POLL_INTERVAL)
        except KeyboardInterrupt:
            break
        except Exception as e:
            logger.error(f"Error: {e}")
            time.sleep(POLL_INTERVAL)


if __name__ == "__main__":
    main()
