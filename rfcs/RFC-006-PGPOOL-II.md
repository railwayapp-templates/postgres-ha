# RFC-006: Pgpool-II - PostgreSQL Middleware for HA and Load Balancing

## Overview

Pgpool-II is a middleware solution that sits between PostgreSQL clients and servers. It provides connection pooling, load balancing, automatic failover, read/write splitting, and query caching. Unlike other HA solutions that focus solely on failover, Pgpool-II acts as a full proxy layer with extensive query routing capabilities.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                     Client Applications                         │
└─────────────────────────────────────────────────────────────────┘
                              │
                              │ PostgreSQL Protocol
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                      Pgpool-II Cluster                          │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │                    Virtual IP (VIP)                       │  │
│  │                    Managed by Watchdog                    │  │
│  └───────────────────────────────────────────────────────────┘  │
│                              │                                  │
│        ┌─────────────────────┼─────────────────────┐           │
│        ▼                     ▼                     ▼           │
│  ┌───────────┐         ┌───────────┐         ┌───────────┐     │
│  │ Pgpool-II │◄───────►│ Pgpool-II │◄───────►│ Pgpool-II │     │
│  │   Node 1  │Watchdog │   Node 2  │Watchdog │   Node 3  │     │
│  │  (Leader) │ Heartbeat│(Standby) │Heartbeat│(Standby) │     │
│  └─────┬─────┘         └─────┬─────┘         └─────┬─────┘     │
└────────┼─────────────────────┼─────────────────────┼───────────┘
         │                     │                     │
         └─────────────────────┼─────────────────────┘
                               │
         ┌─────────────────────┼─────────────────────┐
         ▼                     ▼                     ▼
   ┌───────────┐         ┌───────────┐         ┌───────────┐
   │PostgreSQL │         │PostgreSQL │         │PostgreSQL │
   │ (Primary) │────────►│ (Standby) │────────►│ (Standby) │
   │           │ Stream  │           │ Stream  │           │
   └───────────┘   Rep   └───────────┘   Rep   └───────────┘
```

## Core Features

### 1. Connection Pooling
- Reuses connections to PostgreSQL backends
- Reduces connection overhead
- Configurable pool size per user/database

### 2. Load Balancing
- Distributes read queries across replicas
- Multiple balancing algorithms
- Session-aware or statement-aware

### 3. Automatic Failover
- Detects backend failures
- Promotes standby to primary
- Updates routing automatically

### 4. Read/Write Splitting
- Routes writes to primary
- Routes reads to standbys
- Query parsing or hint-based

### 5. Watchdog
- HA for Pgpool-II itself
- Virtual IP management
- Leader election among Pgpool nodes

## How It Works

### Query Routing

```
Query Routing Decision Tree:
─────────────────────────────────────────────────────────────────

Client Query
     │
     ▼
┌────────────────┐
│ Parse Query    │
└───────┬────────┘
        │
        ▼
┌────────────────┐     Yes    ┌────────────────┐
│ In Transaction?│────────────►│ Route to same  │
│                │            │ backend        │
└───────┬────────┘            └────────────────┘
        │ No
        ▼
┌────────────────┐     Yes    ┌────────────────┐
│ Is Write Query?│────────────►│ Route to       │
│ (INSERT/UPDATE/│            │ Primary        │
│ DELETE/DDL)   │            └────────────────┘
└───────┬────────┘
        │ No (SELECT)
        ▼
┌────────────────┐     Yes    ┌────────────────┐
│ Has Write      │────────────►│ Route to       │
│ Functions?     │            │ Primary        │
└───────┬────────┘            └────────────────┘
        │ No
        ▼
┌────────────────┐
│ Load Balance   │
│ across Replicas│
└────────────────┘
```

### Connection Pool Management

```
Connection Pool Architecture:
─────────────────────────────────────────────────────────────────

          Client Connections         Backend Connections
              │ │ │                       │ │ │
              │ │ │                       │ │ │
         ┌────┴─┴─┴────┐            ┌────┴─┴─┴────┐
         │   Process   │            │   Backend   │
         │    Pool     │───────────►│    Pool     │
         │             │            │             │
         │ Child Proc 1│            │ PG Conn 1   │
         │ Child Proc 2│            │ PG Conn 2   │
         │ Child Proc N│            │ PG Conn M   │
         └─────────────┘            └─────────────┘

Pool Modes:
- Session: 1 client = 1 backend for session duration
- Transaction: 1 client = 1 backend for transaction
- Statement: 1 client = any backend per statement
```

### Watchdog Mechanism

```
Watchdog Architecture:
─────────────────────────────────────────────────────────────────

          ┌─────────────────────────────────────────┐
          │          Virtual IP (VIP)               │
          │         192.168.1.100                   │
          └─────────────────────────────────────────┘
                              │
                              │ Held by Leader
                              ▼
   ┌─────────────┐     ┌─────────────┐     ┌─────────────┐
   │  Pgpool 1   │     │  Pgpool 2   │     │  Pgpool 3   │
   │  (Leader)   │◄───►│  (Standby)  │◄───►│  (Standby)  │
   │  Holds VIP  │     │             │     │             │
   └─────────────┘     └─────────────┘     └─────────────┘
          ▲                   ▲                   ▲
          │                   │                   │
          └───────────────────┴───────────────────┘
                    Heartbeat / Quorum

If Leader fails:
1. Standby nodes detect via heartbeat
2. Election among remaining nodes
3. New leader takes VIP
4. Clients reconnect to VIP
```

### Backend Health Checking

```
Health Check Process:
─────────────────────────────────────────────────────────────────

Every health_check_period seconds:

1. Connect to each backend
   - Uses health_check_user credentials
   - Executes health_check_database connection

2. Optionally run health check query
   - SELECT 1;
   - Or custom query

3. Check streaming replication delay
   - Query pg_stat_replication on primary
   - Compare with sr_check_period

4. Update backend status:
   - UP: Healthy
   - DOWN: Failed checks
   - WAITING: Failover in progress
   - QUARANTINE: Suspicious state
```

## Configuration

### pgpool.conf (Main Configuration)

```ini
# Connection Settings
listen_addresses = '*'
port = 5432
socket_dir = '/var/run/pgpool'

# Backend Configuration
backend_hostname0 = 'pg-primary'
backend_port0 = 5432
backend_weight0 = 1
backend_data_directory0 = '/var/lib/postgresql/16/main'
backend_flag0 = 'ALLOW_TO_FAILOVER'

backend_hostname1 = 'pg-standby1'
backend_port1 = 5432
backend_weight1 = 1
backend_data_directory1 = '/var/lib/postgresql/16/main'
backend_flag1 = 'ALLOW_TO_FAILOVER'

backend_hostname2 = 'pg-standby2'
backend_port2 = 5432
backend_weight2 = 1
backend_data_directory2 = '/var/lib/postgresql/16/main'
backend_flag2 = 'ALLOW_TO_FAILOVER'

# Connection Pooling
num_init_children = 32
max_pool = 4
child_life_time = 300
connection_life_time = 0
client_idle_limit = 0

# Load Balancing
load_balance_mode = on
ignore_leading_white_space = on
read_only_function_list = ''
write_function_list = 'currval,lastval,nextval,setval'

# Streaming Replication Mode
master_slave_mode = on
master_slave_sub_mode = 'stream'
sr_check_period = 10
sr_check_user = 'replication_user'
sr_check_password = 'password'
sr_check_database = 'postgres'
delay_threshold = 10000000  # 10MB lag threshold

# Health Check
health_check_period = 5
health_check_timeout = 20
health_check_user = 'pgpool'
health_check_password = 'password'
health_check_database = 'postgres'
health_check_max_retries = 3
health_check_retry_delay = 1

# Failover
failover_command = '/etc/pgpool-II/failover.sh %d %h %p %D %m %H %M %P %r %R %N %S'
follow_primary_command = '/etc/pgpool-II/follow_primary.sh %d %h %p %D %m %H %M %P %r %R'

# Watchdog
use_watchdog = on
wd_hostname = 'pgpool1'
wd_port = 9000
wd_priority = 1

delegate_IP = '192.168.1.100'
if_cmd_path = '/sbin'
if_up_cmd = '/usr/bin/sudo /sbin/ip addr add $_IP_$/24 dev eth0 label eth0:0'
if_down_cmd = '/usr/bin/sudo /sbin/ip addr del $_IP_$/24 dev eth0'
arping_path = '/usr/sbin'
arping_cmd = '/usr/bin/sudo /usr/sbin/arping -U $_IP_$ -w 1 -I eth0'

# Other Watchdog Nodes
other_pgpool_hostname0 = 'pgpool2'
other_pgpool_port0 = 5432
other_wd_port0 = 9000

other_pgpool_hostname1 = 'pgpool3'
other_pgpool_port1 = 5432
other_wd_port1 = 9000

wd_heartbeat_port = 9694
heartbeat_destination0 = 'pgpool2'
heartbeat_destination_port0 = 9694
heartbeat_destination1 = 'pgpool3'
heartbeat_destination_port1 = 9694
```

### Failover Script (failover.sh)

```bash
#!/bin/bash
# Failover command executed by Pgpool-II

FAILED_NODE_ID=$1
FAILED_HOST=$2
FAILED_PORT=$3
FAILED_DATA_DIR=$4
NEW_PRIMARY_NODE_ID=$5
NEW_PRIMARY_HOST=$6
NEW_PRIMARY_PORT=$7
OLD_PRIMARY_NODE_ID=$8
NEW_PRIMARY_DATA_DIR=$9
OLD_PRIMARY_HOST=${10}

PGHOME=/usr/lib/postgresql/16
PGUSER=postgres

# If the failed node was the primary
if [ $FAILED_NODE_ID = $OLD_PRIMARY_NODE_ID ]; then
    echo "Primary node failed. Promoting standby."

    # Promote new primary
    ssh -T ${NEW_PRIMARY_HOST} "${PGHOME}/bin/pg_ctl promote -D ${NEW_PRIMARY_DATA_DIR}"

    if [ $? -ne 0 ]; then
        echo "Promotion failed!"
        exit 1
    fi

    echo "Promotion successful. New primary: ${NEW_PRIMARY_HOST}"
fi

exit 0
```

### Follow Primary Script (follow_primary.sh)

```bash
#!/bin/bash
# Repoint standbys to new primary after failover

NODE_ID=$1
NODE_HOST=$2
NODE_PORT=$3
NODE_DATA_DIR=$4
NEW_PRIMARY_NODE_ID=$5
NEW_PRIMARY_HOST=$6
NEW_PRIMARY_PORT=$7
OLD_PRIMARY_NODE_ID=$8
NEW_PRIMARY_DATA_DIR=$9
OLD_PRIMARY_HOST=${10}

PGHOME=/usr/lib/postgresql/16

if [ $NODE_ID = $NEW_PRIMARY_NODE_ID ]; then
    # This is the new primary, skip
    exit 0
fi

echo "Reconfiguring standby ${NODE_HOST} to follow new primary ${NEW_PRIMARY_HOST}"

# Stop PostgreSQL
ssh -T ${NODE_HOST} "${PGHOME}/bin/pg_ctl stop -D ${NODE_DATA_DIR} -m fast"

# Use pg_rewind or pg_basebackup
ssh -T ${NODE_HOST} "${PGHOME}/bin/pg_rewind \
    --target-pgdata=${NODE_DATA_DIR} \
    --source-server='host=${NEW_PRIMARY_HOST} port=${NEW_PRIMARY_PORT} user=replication_user'"

# Update primary_conninfo
ssh -T ${NODE_HOST} "echo \"primary_conninfo = 'host=${NEW_PRIMARY_HOST} port=${NEW_PRIMARY_PORT} user=replication_user'\" > ${NODE_DATA_DIR}/postgresql.auto.conf"

# Create standby.signal
ssh -T ${NODE_HOST} "touch ${NODE_DATA_DIR}/standby.signal"

# Start PostgreSQL
ssh -T ${NODE_HOST} "${PGHOME}/bin/pg_ctl start -D ${NODE_DATA_DIR}"

exit 0
```

## Happy Path Scenarios

### Scenario 1: Normal Read/Write Operations

```
Timeline: Application performing mixed workload
─────────────────────────────────────────────────────────────────

Client: BEGIN;
        INSERT INTO orders VALUES (...);
        SELECT * FROM products WHERE id = 123;
        COMMIT;

Pgpool Processing:
─────────────────────────────────────────────────────────────────

t=0ms   BEGIN received
        - Transaction started
        - Next query determines primary/standby

t=1ms   INSERT received
        - Write query detected
        - Route to PRIMARY
        - All subsequent queries go to PRIMARY (transaction)

t=5ms   SELECT received
        - In transaction with write
        - Route to PRIMARY (same backend)

t=10ms  COMMIT received
        - Route to PRIMARY
        - Transaction complete
        - Connection returned to pool

Next query (SELECT without transaction):
        - Not in transaction
        - Read query
        - Load balance to STANDBY
```

### Scenario 2: Connection Pooling Benefits

```
Timeline: High connection workload
─────────────────────────────────────────────────────────────────

Without Pgpool:
        - 1000 clients
        - 1000 PostgreSQL connections
        - High memory usage (~10MB per connection)
        - Process spawning overhead

With Pgpool:
        - 1000 clients connect to Pgpool
        - Pgpool maintains 32 children (num_init_children)
        - Each child has 4 backend connections (max_pool)
        - 128 max PostgreSQL connections
        - 8x connection reduction

Connection Flow:
        Client → Pgpool Child → Backend Connection
                    │
                    └─► Connection reused for next client
```

### Scenario 3: Planned Switchover

```
Timeline: Moving primary to different backend
─────────────────────────────────────────────────────────────────

$ pcp_promote_node -n 1  # Promote backend 1

t=0s    Admin initiates promotion
        - Pgpool receives command

t=1s    Pgpool checks backend 1 status
        - Must be healthy standby
        - Replication lag acceptable

t=2s    Pgpool triggers failover_command
        - Script promotes backend 1
        - pg_promote() called

t=5s    Backend 1 becomes primary
        - Accepting writes

t=6s    Pgpool triggers follow_primary_command
        - Backend 0 (old primary) reconfigured
        - pg_rewind or rebuild

t=10s   All backends reconfigured
        - Backend 1: primary
        - Backend 0, 2: standby following backend 1

t=11s   Pgpool updates internal state
        - Routes writes to new primary
        - Load balances reads to standbys

Minimal downtime: ~5 seconds
```

## Unhappy Path Scenarios

### Scenario 1: Primary Backend Failure

```
Timeline: Primary PostgreSQL crashes
─────────────────────────────────────────────────────────────────

t=0s    Primary (backend 0) crashes

t=5s    Health check fails
        - Cannot connect to backend 0
        - Retry 1

t=6s    Retry 2 fails
t=7s    Retry 3 fails

t=8s    Backend 0 marked DOWN
        - health_check_max_retries exceeded

t=9s    Failover initiated
        - failover_command executed
        - Script promotes best standby

t=10s   Backend 1 promoted
        - pg_promote() successful

t=12s   follow_primary_command executed
        - Backend 2 repoints to backend 1

t=15s   Pgpool reconfigures routing
        - Backend 1: primary
        - Backend 2: standby
        - Backend 0: DOWN

t=16s   Service resumed
        - Writes go to backend 1
        - Reads load balanced to 1 and 2

Total failover time: ~16 seconds
Potential data loss: Uncommitted transactions
```

### Scenario 2: Pgpool Leader Failure

```
Timeline: Active Pgpool node crashes
─────────────────────────────────────────────────────────────────

Watchdog cluster: pgpool1 (Leader), pgpool2, pgpool3
VIP: 192.168.1.100 on pgpool1

t=0s    pgpool1 crashes
        - Process terminates
        - VIP becomes unreachable

t=1s    Heartbeat missed
        - pgpool2 and pgpool3 detect

t=3s    Multiple heartbeats missed
        - pgpool1 confirmed dead

t=4s    Election triggered
        - pgpool2 and pgpool3 compete
        - Highest priority wins (or lower IP)

t=5s    pgpool2 wins election
        - Acquires leader role

t=6s    VIP moved to pgpool2
        - if_up_cmd executed
        - arping_cmd broadcasts new MAC

t=8s    Clients reconnect
        - TCP connections were lost
        - Applications retry to VIP
        - Now routes to pgpool2

Total Pgpool failover: ~8 seconds
Application impact: Connection errors, retry needed
```

### Scenario 3: Split Brain in Watchdog

```
Timeline: Network partition between Pgpool nodes
─────────────────────────────────────────────────────────────────

        pgpool1 (Leader)     │     pgpool2, pgpool3
        VIP: 192.168.1.100   │
              │              │           │
              │   partition  │           │
              │              │           │

Quorum: 3 nodes, need >50% = 2

t=0s    Partition occurs

t=3s    pgpool1 side:
        - Only 1 node (itself)
        - Quorum lost (1 < 2)
        - Must release VIP

t=3s    pgpool2/pgpool3 side:
        - 2 nodes present
        - Quorum maintained (2 >= 2)
        - Can elect new leader

t=5s    pgpool1 releases VIP
        - if_down_cmd executed
        - No longer serving traffic

t=6s    pgpool2 elected leader
        - Takes VIP
        - Continues service

Result: No split brain
        Minority partition loses VIP
        Majority continues operating
```

### Scenario 4: All Standbys Fail

```
Timeline: Cascading standby failures
─────────────────────────────────────────────────────────────────

t=0s    Initial state:
        - Backend 0: PRIMARY
        - Backend 1: STANDBY
        - Backend 2: STANDBY

t=5s    Backend 1 fails
        - Health check fails
        - Marked DOWN
        - Load balancing continues to 0 and 2

t=10s   Backend 2 fails
        - Health check fails
        - Marked DOWN
        - Only backend 0 (PRIMARY) available

t=15s   Current state:
        - Backend 0: PRIMARY (only available)
        - All reads and writes go to PRIMARY
        - No HA capability

Impact:
        - Service continues (degraded)
        - No read scaling
        - If PRIMARY fails, total outage

Recovery:
        - Fix standby nodes
        - pcp_attach_node to reattach
        - Load balancing resumes
```

### Scenario 5: Replication Lag Threshold Exceeded

```
Timeline: Standby falls behind
─────────────────────────────────────────────────────────────────

Configuration:
        delay_threshold = 10000000  # 10MB

t=0s    Normal operation
        - All standbys within threshold
        - Load balancing active

t=30s   Backend 1 starts lagging
        - Network issues
        - Slow disk

t=60s   sr_check detects lag
        - Backend 1 lag: 50MB > 10MB threshold

t=61s   Backend 1 deweighted
        - Removed from load balancing
        - Still available for HA/failover

t=62s   Traffic redistribution
        - Reads go to PRIMARY and backend 2 only
        - Application sees consistent data

When backend 1 catches up:
        - Lag drops below threshold
        - Automatically re-added to load balancing

Impact: Graceful handling of lagging replica
        No stale reads served
```

### Scenario 6: Failover Script Failure

```
Timeline: Failover command fails
─────────────────────────────────────────────────────────────────

t=0s    Primary (backend 0) fails

t=8s    Failover initiated
        - failover_command executed

t=10s   Script fails
        - SSH timeout
        - Permission denied
        - pg_promote fails

t=11s   Failover marked failed
        - Error logged
        - Pgpool in inconsistent state

t=12s   Manual intervention required
        - Check failover script
        - Manual promotion
        - pcp_attach_node / pcp_promote_node

Impact: Extended outage
        No automatic recovery from script failure

Prevention:
        - Test failover scripts thoroughly
        - Ensure SSH keys / permissions
        - Monitor script execution
```

## Tradeoffs

### Advantages

| Aspect | Benefit |
|--------|---------|
| **All-in-One** | Pooling + HA + Load Balancing |
| **Read Scaling** | Automatic read distribution |
| **Query Routing** | Intelligent write/read splitting |
| **Mature** | 20+ years of development |
| **Connection Efficiency** | Significant connection reduction |
| **Watchdog** | Built-in Pgpool HA |
| **Query Cache** | Optional in-memory caching |

### Disadvantages

| Aspect | Limitation |
|--------|------------|
| **Complexity** | Many configuration options |
| **Single Point** | Without watchdog, Pgpool is SPOF |
| **Latency** | Additional hop adds latency |
| **Query Parsing** | Parse overhead for routing |
| **Memory Usage** | Pool processes consume RAM |
| **Script Dependency** | Failover relies on shell scripts |
| **Limited Protocol** | Some PostgreSQL features unsupported |

## Limitations

1. **Query Parsing Overhead**: Every query parsed for routing decisions
2. **Prepared Statement Issues**: Complex handling in load balance mode
3. **Large Object Limitations**: LOB operations may not route correctly
4. **LISTEN/NOTIFY**: May not work correctly with load balancing
5. **Session State**: Some session state not preserved in statement mode
6. **Temporary Tables**: Issues in load balanced configurations
7. **Script Complexity**: Failover scripts can become complex
8. **Debugging Difficulty**: Multiple layers to troubleshoot

## Connection Pool Modes Comparison

| Mode | Connection Reuse | Session State | Best For |
|------|------------------|---------------|----------|
| Session | Per client session | Preserved | General use |
| Transaction | Per transaction | Lost after commit | Web apps |
| Statement | Per statement | Lost after statement | Simple queries |

## Comparison with Alternatives

| Feature | Pgpool-II | PgBouncer | HAProxy |
|---------|-----------|-----------|---------|
| Connection Pooling | Yes | Yes | No |
| Load Balancing | Yes (query-aware) | No | Yes (TCP) |
| Automatic Failover | Yes | No | Health check only |
| Read/Write Split | Yes | No | Limited |
| Query Cache | Yes | No | No |
| HA for itself | Watchdog | External | Keepalived |
| Protocol | PostgreSQL | PostgreSQL | TCP |

## Best Practices

1. **Use Watchdog**: Deploy 3+ Pgpool nodes with watchdog
2. **Test Failover Scripts**: Thoroughly test in staging
3. **Monitor Health Checks**: Alert on backend status changes
4. **Tune Pool Sizes**: Match backend max_connections
5. **Use delay_threshold**: Prevent stale reads
6. **Separate Networks**: Dedicate network for watchdog heartbeat
7. **Log Everything**: Enable detailed logging for debugging
8. **Consider PgBouncer**: For pure pooling without HA needs

## Conclusion

Pgpool-II is the most feature-rich PostgreSQL middleware, combining connection pooling, load balancing, and automatic failover. Its query-aware routing enables intelligent read/write splitting. The main challenges are configuration complexity and the dependency on shell scripts for failover. The watchdog feature provides HA for Pgpool itself, making it a complete solution.

**Recommended for:**
- Applications needing read scaling with write/read split
- Environments requiring connection pooling + HA
- Legacy applications that can't handle multi-host connections
- Teams comfortable with complex configuration

**Not recommended for:**
- Simple HA-only requirements (use Patroni instead)
- Pure connection pooling (use PgBouncer)
- Cloud-native/Kubernetes deployments
- Teams wanting simple, minimal configuration
