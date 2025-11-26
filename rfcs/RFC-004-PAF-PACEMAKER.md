# RFC-004: PAF - PostgreSQL Automatic Failover with Pacemaker

## Overview

PAF (PostgreSQL Automatic Failover) is a resource agent for the Pacemaker cluster manager. It integrates PostgreSQL with the Linux-HA (High Availability) stack, leveraging Pacemaker for cluster coordination and Corosync for messaging. PAF manages PostgreSQL streaming replication, automatic failover, and fencing. This is a traditional enterprise HA approach used in critical infrastructure.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                     Client Applications                         │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                      Virtual IP (VIP)                           │
│              Managed by Pacemaker IPaddr2 resource             │
└─────────────────────────────────────────────────────────────────┘
                              │
        ┌─────────────────────┼─────────────────────┐
        ▼                     ▼                     ▼
┌───────────────┐     ┌───────────────┐     ┌───────────────┐
│    Node 1     │     │    Node 2     │     │    Node 3     │
│  ┌─────────┐  │     │  ┌─────────┐  │     │  ┌─────────┐  │
│  │Pacemaker│  │     │  │Pacemaker│  │     │  │Pacemaker│  │
│  │  (crmd) │  │     │  │  (crmd) │  │     │  │  (crmd) │  │
│  └────┬────┘  │     │  └────┬────┘  │     │  └────┬────┘  │
│       │       │     │       │       │     │       │       │
│  ┌────▼────┐  │     │  ┌────▼────┐  │     │  ┌────▼────┐  │
│  │   PAF   │  │     │  │   PAF   │  │     │  │   PAF   │  │
│  │ (RA)    │  │     │  │ (RA)    │  │     │  │ (RA)    │  │
│  └────┬────┘  │     │  └────┬────┘  │     │  └────┬────┘  │
│       │       │     │       │       │     │       │       │
│  ┌────▼────┐  │     │  ┌────▼────┐  │     │  ┌────▼────┐  │
│  │PostgreSQL│ │     │  │PostgreSQL│ │     │  │PostgreSQL│ │
│  │(Primary) │ │     │  │(Standby) │ │     │  │(Standby) │ │
│  └─────────┘  │     │  └─────────┘  │     │  └─────────┘  │
└───────────────┘     └───────────────┘     └───────────────┘
        │                     │                     │
        └─────────────────────┼─────────────────────┘
                              │
                      Corosync Ring
                  (Cluster Communication)
```

## Linux-HA Stack Components

### 1. Corosync
- Cluster communication layer
- Provides reliable messaging between nodes
- Handles membership and quorum
- Uses totem protocol for ring communication
- Detects node failures via heartbeat

### 2. Pacemaker
- Cluster Resource Manager (CRM)
- Makes decisions about resource placement
- Handles failover logic
- Manages constraints and colocation
- Provides stonith (fencing) framework

### 3. PAF Resource Agent
- OCF-compliant resource agent for PostgreSQL
- Interfaces between Pacemaker and PostgreSQL
- Monitors PostgreSQL state
- Executes promotion and demotion
- Reports replication status

### 4. STONITH/Fencing
- "Shoot The Other Node In The Head"
- Ensures failed nodes are truly offline
- Prevents split-brain scenarios
- Required for production deployments

## How It Works

### Cluster Communication (Corosync)

```
Corosync Totem Ring:
─────────────────────────────────────────────────────────────────

Node1 ─────────► Node2 ─────────► Node3
  ▲                                  │
  │                                  │
  └──────────────────────────────────┘

- Token passed around ring
- Node must have token to transmit
- Missing heartbeat = suspected failure
- Quorum calculated from visible nodes
```

### Resource State Machine

```
PAF Resource States:
─────────────────────────────────────────────────────────────────

            ┌───────────────┐
            │    STOPPED    │
            └───────┬───────┘
                    │ start
                    ▼
            ┌───────────────┐
            │    SLAVE      │◄────────────┐
            │  (streaming)  │             │
            └───────┬───────┘             │
                    │ promote             │ demote
                    ▼                     │
            ┌───────────────┐             │
            │    MASTER     │─────────────┘
            │  (primary)    │
            └───────────────┘

Master-Slave resource type:
- Exactly one Master at a time
- Multiple Slaves allowed
- Pacemaker enforces constraint
```

### Failover Decision Process

```
Pacemaker Failover Logic:
─────────────────────────────────────────────────────────────────

1. Monitor operation fails for Master
   - PAF returns OCF_ERR_GENERIC
   - Or node heartbeat lost

2. Pacemaker detects failure
   - Marks Master resource as failed
   - Triggers recovery

3. STONITH fence (if configured)
   - Fence failed node first
   - Ensure it cannot write

4. Evaluate promotion candidates
   - Check Slave scores
   - Consider constraints
   - Select best candidate

5. Promote selected Slave
   - PAF executes promotion
   - pg_promote() called

6. Update VIP
   - Move IP to new Master
   - Clients reconnect

Timeline: 10-30 seconds typical
```

## Configuration

### Corosync Configuration (/etc/corosync/corosync.conf)

```
totem {
    version: 2
    cluster_name: postgres-ha
    transport: knet

    crypto_cipher: aes256
    crypto_hash: sha256
}

nodelist {
    node {
        ring0_addr: 192.168.1.101
        name: node1
        nodeid: 1
    }
    node {
        ring0_addr: 192.168.1.102
        name: node2
        nodeid: 2
    }
    node {
        ring0_addr: 192.168.1.103
        name: node3
        nodeid: 3
    }
}

quorum {
    provider: corosync_votequorum
    expected_votes: 3
    two_node: 0
}

logging {
    to_logfile: yes
    logfile: /var/log/corosync/corosync.log
    to_syslog: yes
}
```

### Pacemaker Resource Configuration

```bash
# Create PostgreSQL resource
pcs resource create pgsql ocf:heartbeat:pgsqld \
    pgctl="/usr/pgsql-16/bin/pg_ctl" \
    psql="/usr/pgsql-16/bin/psql" \
    pgdata="/var/lib/pgsql/16/data" \
    pgport="5432" \
    op start timeout=60s \
    op stop timeout=60s \
    op promote timeout=30s \
    op demote timeout=120s \
    op monitor interval=15s timeout=10s role="Master" \
    op monitor interval=16s timeout=10s role="Slave" \
    op notify timeout=60s

# Or using PAF specifically
pcs resource create pgsql-ha ocf:resource-agents:pgsqlms \
    bindir="/usr/pgsql-16/bin" \
    pgdata="/var/lib/pgsql/16/data" \
    recovery_template="/var/lib/pgsql/recovery.conf.pcmk" \
    op start timeout=60s \
    op stop timeout=60s \
    op promote timeout=30s \
    op demote timeout=120s \
    op monitor interval=15s timeout=10s role="Master" \
    op monitor interval=16s timeout=10s role="Slave"

# Create master-slave resource
pcs resource promotable pgsql-ha \
    meta notify=true \
    master-max=1 \
    master-node-max=1 \
    clone-max=3 \
    clone-node-max=1

# Create VIP resource
pcs resource create pgsql-vip ocf:heartbeat:IPaddr2 \
    ip="192.168.1.100" \
    cidr_netmask="24" \
    op monitor interval=10s

# Colocation: VIP follows Master
pcs constraint colocation add pgsql-vip with master pgsql-ha-clone INFINITY

# Order: Promote before VIP
pcs constraint order promote pgsql-ha-clone then start pgsql-vip symmetrical=false
```

### STONITH (Fencing) Configuration

```bash
# For IPMI/iLO fencing
pcs stonith create ipmi-fence-node1 fence_ipmilan \
    pcmk_host_list="node1" \
    ipaddr="192.168.1.201" \
    login="admin" \
    passwd="password" \
    lanplus=1 \
    op monitor interval=60s

# For cloud environments (AWS)
pcs stonith create aws-fence fence_aws \
    region="us-east-1" \
    access_key="AKIA..." \
    secret_key="..." \
    pcmk_host_map="node1:i-xxxxx;node2:i-yyyyy"

# For VMware
pcs stonith create vmware-fence fence_vmware_soap \
    ipaddr="vcenter.example.com" \
    login="admin@vsphere.local" \
    passwd="..." \
    pcmk_host_map="node1:vm-node1;node2:vm-node2"
```

### PAF-Specific Configuration

```bash
# /etc/paf/pgsqlms.conf (PAF resource agent config)

# PostgreSQL paths
PGDATA=/var/lib/pgsql/16/data
PGHOST=/var/run/postgresql
PGPORT=5432
PGUSER=postgres

# Recovery template for standbys
RECOVERY_TEMPLATE=/var/lib/pgsql/recovery.conf.template

# Monitoring
MASTER_SCORE=1000
STANDBY_SCORE=100
STOPPED_SCORE=0

# Replication lag threshold (bytes)
MAX_LAG=16777216
```

## Happy Path Scenarios

### Scenario 1: Cluster Startup

```
Timeline: Starting a fresh cluster
─────────────────────────────────────────────────────────────────

t=0s    Start Corosync on all nodes
        $ systemctl start corosync
        - Ring forms
        - Quorum established

t=5s    Start Pacemaker on all nodes
        $ systemctl start pacemaker
        - CRM connects to Corosync
        - Resources discovered

t=10s   Pacemaker evaluates resources
        - No Master running
        - Selects node for Master

t=15s   PAF starts PostgreSQL as Master on node1
        - pg_ctl start
        - Returns OCF_SUCCESS

t=20s   PAF starts PostgreSQL as Slave on node2, node3
        - Connects to Master
        - Begins streaming

t=25s   VIP assigned to Master node
        - IPaddr2 brings up 192.168.1.100 on node1

t=30s   Cluster operational
        - Monitoring active
        - All resources running

$ pcs status
Cluster name: postgres-ha
Status of pacemakerd: 'Pacemaker is running'

Node List:
  * Online: [ node1 node2 node3 ]

Full List of Resources:
  * Clone Set: pgsql-ha-clone [pgsql-ha] (promotable):
    * Masters: [ node1 ]
    * Slaves: [ node2 node3 ]
  * pgsql-vip (ocf:heartbeat:IPaddr2): Started node1
```

### Scenario 2: Planned Switchover

```
Timeline: Moving Master to different node
─────────────────────────────────────────────────────────────────

$ pcs resource move pgsql-ha-clone node2 --master

t=0s    Administrator initiates move
        - Pacemaker creates location constraint

t=1s    Pacemaker plans transition
        - Demote node1
        - Promote node2
        - Move VIP

t=2s    VIP removed from node1
        - Clients disconnect momentarily

t=3s    Master demoted on node1
        - PostgreSQL enters read-only
        - Checkpoint completed

t=5s    Slave promoted on node2
        - pg_promote() executed
        - New timeline

t=7s    Old Master becomes Slave
        - Reconfigured to follow node2
        - Streaming resumes

t=10s   VIP started on node2
        - Clients can reconnect

t=12s   Clean up constraint
        $ pcs resource clear pgsql-ha-clone

Total downtime: ~8-10 seconds
```

### Scenario 3: Normal Monitoring

```
Timeline: Steady state monitoring
─────────────────────────────────────────────────────────────────

Every 15s (Master monitor):
    PAF checks:
    - PostgreSQL is running
    - Accepting connections
    - Is in recovery = false
    - Returns: OCF_RUNNING_MASTER

Every 16s (Slave monitor):
    PAF checks:
    - PostgreSQL is running
    - Is in recovery = true
    - Replication lag < threshold
    - Returns: OCF_RUNNING_SLAVE

Corosync continuously:
    - Exchanges heartbeats
    - Maintains quorum
    - Token circulation
```

## Unhappy Path Scenarios

### Scenario 1: Master Node Failure

```
Timeline: Complete node failure (hardware/kernel panic)
─────────────────────────────────────────────────────────────────

t=0s    Node1 (Master) crashes
        - Kernel panic / power loss
        - No graceful shutdown

t=1s    Corosync detects missing heartbeat
        - Token not received from node1
        - Other nodes continue ring without node1

t=3s    Corosync declares node1 dead
        - Membership view changes
        - Pacemaker notified

t=4s    Pacemaker marks node1 resources as UNKNOWN
        - Master resource status unknown

t=5s    STONITH triggered
        - fence_ipmilan called
        - IPMI power off command sent

t=10s   STONITH confirmed
        - Node1 definitely offline
        - Safe to proceed

t=11s   Pacemaker calculates new configuration
        - Node1 excluded
        - Selects node2 for promotion (highest score)

t=12s   PAF promotes node2
        - pg_promote() called
        - Returns OCF_SUCCESS

t=14s   Node3 reconfigured
        - PAF updates primary_conninfo
        - Replication from node2

t=16s   VIP started on node2
        - IP address activated
        - Clients reconnect

Total failover time: ~16 seconds (with fast STONITH)

Without STONITH: Pacemaker refuses to promote
                 "Manual intervention required"
```

### Scenario 2: PostgreSQL Crashes (Node Healthy)

```
Timeline: PostgreSQL process crash, node still up
─────────────────────────────────────────────────────────────────

t=0s    PostgreSQL Master crashes
        - Process terminated
        - Node1 still running

t=15s   PAF monitor on node1 runs
        - pg_isready fails
        - Returns OCF_NOT_RUNNING

t=16s   Pacemaker detects Master failure
        - Resource marked failed
        - Recovery initiated

t=17s   Pacemaker attempts restart on node1
        - PAF start operation
        - migration-threshold not exceeded

t=20s   If restart succeeds:
        - Master back online
        - No promotion needed

t=20s   If restart fails (migration-threshold=3):
        - After 3 failures, node banned
        - Promotion to node2

Total time (restart success): ~20 seconds
Total time (needs promotion): ~35 seconds
```

### Scenario 3: Network Partition (Split Brain)

```
Timeline: Network splits cluster
─────────────────────────────────────────────────────────────────

3-node cluster with quorum

    node1 (Master)    │    node2, node3 (Slaves)
          │           │         │
        ──┴──  partition ──┴────┴──

    Minority (1)      │    Majority (2)

t=0s    Network partition occurs

t=2s    Corosync detects partition
        - node1 loses contact with node2, node3
        - node2, node3 still see each other

t=3s    Quorum calculation:
        - node1: 1 vote, needs 2, NO QUORUM
        - node2+node3: 2 votes, have quorum, QUORATE

t=4s    node1 (minority side):
        - Loses quorum
        - Pacemaker stops all resources
        - PostgreSQL stopped/fenced
        - No split brain possible

t=5s    node2+node3 (majority side):
        - STONITH node1 (safety)
        - Promote node2 to Master
        - VIP moves to node2

t=15s   Cluster operational in degraded mode
        - Master on node2
        - Slave on node3
        - node1 fenced

Result: No split brain
        Minority partition shut down
        Majority takes over safely
```

### Scenario 4: Quorum Lost (Catastrophic)

```
Timeline: Losing more than half the nodes
─────────────────────────────────────────────────────────────────

3-node cluster:

t=0s    node2 fails
t=5s    node3 fails
t=10s   Only node1 (Master) remains

        node1: 1 vote, expected 3, NO QUORUM

t=11s   Pacemaker policy: no-quorum-policy=stop
        - All resources stopped
        - PostgreSQL shut down
        - VIP removed

Result: Complete service outage
        Prevents potential split brain
        Manual intervention required

Alternative: no-quorum-policy=ignore (DANGEROUS)
        - Resources continue running
        - Risk of split brain
        - Only for specific scenarios
```

### Scenario 5: STONITH Failure

```
Timeline: Fencing device unavailable
─────────────────────────────────────────────────────────────────

t=0s    Master node1 becomes unresponsive

t=3s    Corosync declares node1 dead

t=5s    Pacemaker triggers STONITH
        - fence_ipmilan called
        - IPMI unreachable/timeout

t=35s   STONITH timeout (30s default)
        - Fencing failed

t=36s   Pacemaker cannot proceed
        - stonith-enabled=true (production setting)
        - Cannot safely promote Slave
        - Cluster in ERROR state

Manual intervention required:
        1. Physically verify node1 is down
        2. $ pcs stonith confirm node1
        3. Pacemaker continues with promotion

Impact: Extended outage until manual confirmation
        Safety preserved (no split brain)
```

### Scenario 6: Replication Lag Exceeds Threshold

```
Timeline: Standby too far behind for safe promotion
─────────────────────────────────────────────────────────────────

Configuration: MAX_LAG=16777216 (16MB)

t=0s    Master heavily loaded
        - WAL generation: 100MB/s
        - Standby lag: 500MB

t=10s   Master crashes

t=15s   Pacemaker evaluates promotion candidates
        - node2 lag: 500MB > 16MB threshold
        - node3 lag: 480MB > 16MB threshold

PAF behavior depends on configuration:

Option A: Strict mode
        - No node meets criteria
        - Cluster remains without Master
        - Manual intervention required

Option B: Best-effort mode
        - Promote least-lagged node
        - Accept data loss
        - Log warning

Impact: Data loss vs availability tradeoff
```

## Tradeoffs

### Advantages

| Aspect | Benefit |
|--------|---------|
| **Enterprise Proven** | Decades of Linux-HA experience |
| **True Fencing** | STONITH ensures safety |
| **Quorum** | Proper consensus for partitions |
| **VIP Built-in** | No external load balancer needed |
| **Flexible** | Supports complex constraint logic |
| **Hardware Agnostic** | Works with any fencing device |
| **Multi-Resource** | Can manage entire stack (PostgreSQL + VIP + app) |

### Disadvantages

| Aspect | Limitation |
|--------|------------|
| **Complexity** | Steep learning curve |
| **Configuration** | XML/pcs commands verbose |
| **Not Cloud Native** | Difficult in containerized environments |
| **STONITH Required** | Must have fencing mechanism |
| **Resource Overhead** | Corosync + Pacemaker + agents |
| **No Kubernetes** | Not designed for K8s |
| **Older Paradigm** | Traditional datacenter approach |

## Key Parameters

### Timing Parameters

```bash
# Resource timeouts
op start timeout=60s      # Time to start PostgreSQL
op stop timeout=60s       # Time to stop PostgreSQL
op promote timeout=30s    # Time to promote
op demote timeout=120s    # Time to demote (includes checkpoint)
op monitor interval=15s   # Monitoring frequency

# STONITH timeout
stonith-timeout=30s       # Max time for fencing operation

# Migration threshold
migration-threshold=3     # Failures before moving resource
```

### Quorum Settings

```
# Corosync quorum
expected_votes: 3         # Total nodes in cluster
two_node: 0               # Enable for 2-node (needs special handling)

# Pacemaker quorum policy
no-quorum-policy=stop     # stop|freeze|ignore|suicide
```

## Comparison with Alternatives

| Feature | PAF/Pacemaker | Patroni | repmgr |
|---------|---------------|---------|--------|
| External DCS | No (Corosync built-in) | Yes | No |
| Fencing | STONITH (comprehensive) | Limited | Script-based |
| Quorum | Corosync native | DCS-based | Location-based |
| Complexity | High | Medium | Low |
| Cloud Support | Poor | Good | Medium |
| Kubernetes | No | Yes | No |
| VIP Management | Native | External | External |

## Limitations

1. **Operational Complexity**: Requires deep Linux-HA knowledge
2. **Not Cloud Native**: Difficult with dynamic infrastructure
3. **STONITH Mandatory**: Must have working fencing
4. **No Kubernetes**: Doesn't fit container orchestration model
5. **Configuration Verbose**: XML-based, complex constraints
6. **Debugging Difficult**: Multi-layer stack to troubleshoot
7. **Resource Overhead**: Multiple daemons per node
8. **Legacy Architecture**: Designed for traditional datacenters

## Best Practices

1. **Always Configure STONITH**: Never disable in production
2. **Use Odd Node Counts**: 3 or 5 nodes for quorum
3. **Test Fencing**: Verify STONITH works before going live
4. **Monitor Corosync**: Alert on ring/quorum issues
5. **Document Configuration**: Complex configs need documentation
6. **Regular Failover Testing**: Verify promotion works
7. **Separate Networks**: Dedicated network for Corosync
8. **Redundant Fencing**: Multiple STONITH devices

## Conclusion

PAF with Pacemaker represents the traditional enterprise approach to PostgreSQL HA. It provides robust failover with true fencing and quorum support. The main challenges are operational complexity and poor fit for modern cloud-native environments. This solution is best suited for organizations with existing Linux-HA expertise and traditional datacenter infrastructure.

**Recommended for:**
- Traditional datacenter deployments
- Organizations with Linux-HA expertise
- Environments requiring strong fencing guarantees
- Mixed workloads (PostgreSQL + other services)

**Not recommended for:**
- Cloud-native/Kubernetes deployments
- Teams new to HA concepts
- Dynamic/elastic infrastructure
- Containerized environments
