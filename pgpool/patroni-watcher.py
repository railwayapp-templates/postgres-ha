#!/usr/bin/env python3
"""
Patroni Watcher - Monitors Patroni cluster state and updates Pgpool-II backends
Polls Patroni REST API and uses PCP to detach/attach backends based on leader changes
"""

import os
import sys
import time
import subprocess
import logging
from typing import Optional, Dict, List
import requests
from requests.exceptions import RequestException, Timeout, ConnectionError

# Configure logging to stderr
logging.basicConfig(
    level=logging.INFO,
    format='[patroni-watcher] %(levelname)s: %(message)s',
    stream=sys.stderr
)
logger = logging.getLogger(__name__)

# Configuration
PATRONI_BACKENDS = [
    {"name": "postgres-1", "host": "postgres-1.railway.internal", "port": 5432, "index": 0},
    {"name": "postgres-2", "host": "postgres-2.railway.internal", "port": 5432, "index": 1},
    {"name": "postgres-3", "host": "postgres-3.railway.internal", "port": 5432, "index": 2},
]

PCP_HOST = "localhost"
PCP_PORT = 9898
PCP_USER = os.environ.get("PGPOOL_ADMIN_USERNAME", "admin")
PCP_PASSWORD = os.environ.get("PGPOOL_ADMIN_PASSWORD", "")
POLL_INTERVAL = 2
PATRONI_TIMEOUT = 3  # seconds for HTTP requests


def run_pcp_command(cmd: str, *args) -> bool:
    """Execute a PCP command with proper environment and error handling"""
    try:
        env = os.environ.copy()
        # PCP uses PCPPASSFILE pointing to a password file, not a direct password env var
        env["PCPPASSFILE"] = "/tmp/.pcppass"

        full_cmd = [
            cmd,
            "-h", PCP_HOST,
            "-p", str(PCP_PORT),
            "-U", PCP_USER,
            "-w",
            *args
        ]

        logger.debug(f"Running PCP command: {' '.join(full_cmd)}")
        result = subprocess.run(
            full_cmd,
            env=env,
            capture_output=True,
            text=True,
            timeout=10
        )

        if result.returncode != 0:
            logger.warning(f"PCP command failed (exit {result.returncode}): stdout={result.stdout.strip()}, stderr={result.stderr.strip()}")
            return False

        logger.debug(f"PCP command succeeded: {result.stdout.strip()}")
        return True
    except subprocess.TimeoutExpired:
        logger.error(f"PCP command timed out: {cmd}")
        return False
    except Exception as e:
        logger.error(f"Error running PCP command {cmd}: {e}")
        return False


def get_patroni_role(host: str, port: int = 8008) -> Optional[str]:
    """
    Query Patroni REST API to get the role of a node
    Returns 'master', 'replica', or None on error
    """
    url = f"http://{host}:{port}/"

    try:
        logger.debug(f"Querying Patroni API: {url}")
        response = requests.get(url, timeout=PATRONI_TIMEOUT)

        if response.status_code != 200:
            logger.debug(f"{host}: HTTP {response.status_code}")
            return None

        data = response.json()
        role = data.get("role")

        if not role:
            logger.debug(f"{host}: No 'role' field in response: {data}")
            return None

        logger.debug(f"{host}: role is '{role}'")
        return role

    except Timeout:
        logger.debug(f"{host}: Request timed out after {PATRONI_TIMEOUT}s")
        return None
    except ConnectionError as e:
        logger.debug(f"{host}: Connection error: {e}")
        return None
    except requests.exceptions.JSONDecodeError:
        logger.debug(f"{host}: Invalid JSON response")
        return None
    except RequestException as e:
        logger.debug(f"{host}: Request failed: {e}")
        return None
    except Exception as e:
        logger.error(f"{host}: Unexpected error: {e}")
        return None


def get_cluster_leader() -> Optional[Dict]:
    """
    Poll all backends to find the current leader
    Returns backend dict or None if no leader found
    """
    for backend in PATRONI_BACKENDS:
        role = get_patroni_role(backend["host"])

        if role in ("master", "primary"):
            return backend

    return None


def detach_backend(index: int):
    """Detach a backend from pgpool"""
    logger.info(f"Detaching backend {index}")
    run_pcp_command("pcp_detach_node", "-n", str(index))


def attach_backend(index: int):
    """Attach a backend to pgpool"""
    logger.info(f"Attaching backend {index}")
    run_pcp_command("pcp_attach_node", "-n", str(index))


def sync_pgpool_with_patroni(leader: Dict):
    """
    Sync pgpool backends with Patroni state
    Attach the leader, detach all replicas
    """
    leader_name = leader["name"]
    logger.info(f"Syncing pgpool: leader is {leader_name}")

    for backend in PATRONI_BACKENDS:
        if backend["name"] == leader_name:
            attach_backend(backend["index"])
        else:
            detach_backend(backend["index"])


def wait_for_pgpool():
    """Wait for pgpool PCP port to be ready"""
    logger.info("Waiting for pgpool to be ready...")

    for i in range(60):
        try:
            result = subprocess.run(
                ["timeout", "2", "bash", "-c", "echo quit | nc localhost 9898"],
                capture_output=True,
                timeout=3
            )
            if result.returncode == 0:
                logger.info("Pgpool PCP port is ready")
                return True
        except:
            pass

        time.sleep(2)

    logger.error("Timeout waiting for pgpool to be ready")
    return False


def main():
    logger.info("Starting Patroni watcher")
    logger.info(f"Monitoring backends: {[b['name'] for b in PATRONI_BACKENDS]}")
    logger.info(f"Poll interval: {POLL_INTERVAL}s")

    if not PCP_PASSWORD:
        logger.error("PGPOOL_ADMIN_PASSWORD not set")
        sys.exit(1)

    # Wait for pgpool to be ready
    if not wait_for_pgpool():
        sys.exit(1)

    last_leader_name = None

    while True:
        try:
            logger.debug("Polling cluster for leader...")
            current_leader = get_cluster_leader()

            if current_leader is None:
                logger.warning("No leader found in cluster")
            elif current_leader["name"] != last_leader_name:
                logger.info(f"Leader change detected: {last_leader_name or 'none'} -> {current_leader['name']}")
                sync_pgpool_with_patroni(current_leader)
                last_leader_name = current_leader["name"]
            else:
                logger.debug(f"Leader unchanged: {current_leader['name']}")

            time.sleep(POLL_INTERVAL)

        except KeyboardInterrupt:
            logger.info("Shutting down...")
            break
        except Exception as e:
            logger.error(f"Unexpected error in main loop: {e}", exc_info=True)
            time.sleep(POLL_INTERVAL)


if __name__ == "__main__":
    main()
