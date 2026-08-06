# PostgreSQL High Availability Template for Railway

This template deploys a production-ready, highly-available PostgreSQL cluster on Railway with automatic failover, read replicas, and transparent connection routing.

## Features

- **3-node PostgreSQL cluster** with streaming replication
- **Automatic failover** in <10 seconds using Patroni
- **2 standby replicas** for automatic failover and read scaling
- **HAProxy load balancer** with separate read-write and read-only endpoints
- **Transparent routing** - single endpoint for all database connections
- **Automatic recovery** - failed nodes rejoin as replicas
- **Health monitoring** - Patroni REST API for cluster status
- **Multi-version support** - PostgreSQL 14, 15, 16, 17, and 18

## Architecture

```
Application
    ↓
HAProxy ← Load balanced endpoint
    ├─ :5432 (read-write) → Primary only
    └─ :5433 (read-only)  → Replicas (round-robin)
    ↓
PostgreSQL Cluster
    ├─ postgres-1 (Leader)  ← Writes + Reads
    ├─ postgres-2 (Standby) ← Reads + Failover ready
    └─ postgres-3 (Standby) ← Reads + Failover ready
         ↓
    etcd (3 nodes) ← Distributed consensus
```

## Services Deployed

1. **postgres-1, postgres-2, postgres-3** - PostgreSQL with Patroni orchestration
2. **etcd-1, etcd-2, etcd-3** - Distributed key-value store for leader election
3. **haproxy** - Load balancer with automatic primary detection

**Total**: 7 services

## Point-in-time recovery (opt-in)

The `postgres-patroni` image ships with [pgBackRest](https://pgbackrest.org/)
installed but dormant. When `WAL_ARCHIVE_BUCKET` is unset, the image
behaves identically to a vanilla HA Postgres image — no archiving, no
config mutation. When set, the current Patroni leader archives WAL
continuously to S3-compatible storage in **async mode**
(`archive_mode=on`); standbys carry the same config + binary so promotion
instantly enables archiving with no config change.

The image reads a tool-agnostic `WAL_ARCHIVE_*` / `WAL_RECOVER_FROM_*`
env contract. `patroni-runner` translates `WAL_ARCHIVE_*` to pgBackRest's
native `PGBACKREST_REPO1_S3_*` so the main `pgbackrest.conf`'s repo1
always = the cluster's own archive bucket. `WAL_RECOVER_FROM_*` is
materialised in a separate `/etc/pgbackrest/pgbackrest-recovery-source.conf`
(only that file references the source bucket) and is NEVER exported as
env vars — pgBackRest's option resolution is command-line > env > config
> default, so a global env export would silently override the
`--config` we pass during recovery and could leak archive-push to the
source bucket. With this isolation:

- `WAL_ARCHIVE_*` only → standalone archiving cluster. Main conf's
  repo1 = archive bucket.
- `WAL_RECOVER_FROM_*` only → cluster restoring from a source bucket;
  recovery-source conf has the source. After promote the cluster runs
  as plain non-archiving Postgres.
- `WAL_ARCHIVE_*` + `WAL_RECOVER_FROM_*` → restored cluster with PITR
  re-enabled. Main conf's repo1 = own archive bucket (post-promote
  archive-push); recovery-source conf has source bucket (archive-get
  during replay only). Source and own bucket never appear in the same
  config file, so archive-push from the fork can never spray into the
  source bucket.

Operator-facing env contract:

| Env var | Purpose |
|---|---|
| `WAL_ARCHIVE_BUCKET` | bucket name — gates archiving on this cluster |
| `WAL_ARCHIVE_ENDPOINT` | S3-compatible endpoint (e.g. `fly.storage.tigris.dev`) |
| `WAL_ARCHIVE_REGION` | bucket region |
| `WAL_ARCHIVE_KEY` / `WAL_ARCHIVE_SECRET` | bucket credentials |
| `WAL_ARCHIVE_PATH` | path prefix where archive-push writes (default `/pgbackrest`) |
| `WAL_RECOVER_FROM_BUCKET` / `_ENDPOINT` / `_REGION` / `_KEY` / `_SECRET` / `_PATH` | source-bucket coordinates on a PITR-restored cluster; `archive-get` reads source WAL from here during replay. Set by backboard on restore; not normally a manual knob. |
| `POSTGRES_RECOVERY_TARGET_TIME` | ISO 8601 timestamp; stages archive-recovery replay on next start |
| `POSTGRES_ARCHIVE_TIMEOUT` | seconds Postgres waits before forcing a WAL switch (default `60`) |
| `POSTGRES_BASEBACKUP_MAX_RATE` | `--max-rate` for pg_basebackup replica creation (default `20M`; explicit `k`/`M` suffix required, `32k`..`1024M`; invalid values fall back to the default). Raise for an emergency fast re-seed at the cost of leader disk contention. |
| `WAL_BACKUP_FULL_INTERVAL_HOURS` | image-owned full base-backup cadence (default `168` = weekly; `0` disables periodic fulls). Initial / gap-recovery fulls fire regardless. |
| `WAL_BACKUP_DIFF_INTERVAL_HOURS` | image-owned differential base-backup cadence (default `24`; `0` disables) |
| `WAL_BACKUP_RETENTION_FULL` | full backups kept by `pgbackrest expire` (default `4`) |
| `WAL_BACKUP_RETENTION_DIFF` | differentials kept by `pgbackrest expire` (default `14`) |
| `WAL_HEARTBEAT_DISABLED` | set to `1` to disable the idle-DB WAL heartbeat (advanced; reduces archive cost on quiet DBs at the price of stale PITR ceiling) |

Image-level tuning knobs (pgBackRest-native, internal):

| Env var | Purpose |
|---|---|
| `WAL_DROP_THRESHOLD_MB` | `pg_wal/` size at which the archive-push wrapper drops failing segments to keep Postgres running (default `5120`, matching `archive-push-queue-max`). Outside the `PGBACKREST_*` namespace because pgBackRest treats unknown `PGBACKREST_*` vars as config options and warns about them on every push. |
| `PGBACKREST_ARCHIVE_PUSH_PROCESS_MAX` | parallel workers for `archive-push`. Default auto-sized as `clamp(cpus/8, 2, 8)`. |
| `PGBACKREST_ARCHIVE_GET_PROCESS_MAX` | parallel workers for `archive-get`. Default `1` (WAL replay is serial). |
| `PGBACKREST_BACKUP_PROCESS_MAX` | parallel workers for `backup`. Default auto-sized as `clamp(cpus/4, 1, 16)`. |
| `PGBACKREST_RESTORE_PROCESS_MAX` | parallel workers for `restore`. Default auto-sized as `clamp(cpus, 1, 32)`. |

When `WAL_ARCHIVE_BUCKET` is set, `patroni-runner` writes
`archive_mode=on`, `archive_command='/usr/local/bin/pgbackrest-archive-push-wrapper.sh %p'`,
and `archive_timeout` (default `60`, override via `POSTGRES_ARCHIVE_TIMEOUT`)
into the Patroni-generated cluster config, and renders
`/etc/pgbackrest/pgbackrest.conf` with operator-policy defaults
(`archive-async=y`, `archive-push-queue-max=5GiB`, per-command
`process-max` sized off cgroup-detected vCPU, `compress-type=zst`,
`spool-path=$PGDATA/pgbackrest-spool` so segments staged but not yet
pushed survive container restarts).

Patroni propagates the same config to every node, but under
`archive_mode=on` only the current leader fires `archive_command`. Standbys
hold the pgBackRest binary and config so promotion instantly enables
archiving with no config re-push. Residual failover RPO is `archive_timeout`
(60s) plus failover-detection time; Patroni's promotion logic flushes any
pending `archive_status/.ready` markers on the new leader to close most of
that gap on planned switchover.

**The never-halt guarantee** comes from two thresholds, sized identically
and checked as one combined budget:

- `archive-push-queue-max=5GiB` (image-baked) governs the **spool**.
  Trips when the async worker can't keep up. Generous buffer to absorb
  multi-hour outages — most segments eventually land once it clears.
- `WAL_DROP_THRESHOLD_MB=5120` (default) governs **`pg_wal/` + the spool,
  combined**, when pgbackrest's foreground returns non-zero. The
  `archive_command` is the wrapper script (not pgbackrest directly), which
  sums `pg_wal/` and spool directory sizes on failure and drops segments
  past the threshold to keep Postgres up. Checking the sum — not `pg_wal/`
  alone — matters because the two can each fill for different reasons at
  once (foreground copy-to-spool failing vs. background upload stalled);
  identical-but-independent caps would let a single outage hold up to ~2x
  the intended budget on disk. Only two known no-recovery-possible errors
  (bad creds, deleted bucket) bypass this and drop immediately regardless
  of size — every other failure, including transient S3-side errors, gets
  the same generous combined budget before anything is dropped.

Either threshold tripping truncates the PITR window; the database keeps
running. This is the explicit reason this image uses pgBackRest instead
of wal-g — synchronous `archive_command` shapes halt Postgres on full
`pg_wal`, requiring manual intervention to recover.

`patroni-runner` runs `pgbackrest stanza-create` automatically once
Postgres becomes reachable — idempotent, runs from every node, gated on
`WAL_ARCHIVE_BUCKET` being set (skipped in dual-repo mode so we never
mutate the source's stanza).

**Env-var changes on existing clusters**: `patroni-runner` includes a
one-shot DCS reconcile that runs after Patroni's REST API comes up. When
`WAL_ARCHIVE_BUCKET` is set but DCS lacks archive params (or vice versa),
the reconcile patches DCS via `PATCH /config` so the env-var intent is
authoritative. Because `archive_mode` is `PGC_POSTMASTER`, applying the
patch flags `pending_restart` on the node — operators must run
`patronictl restart <cluster> <node>` (or trigger one redeploy per node
through Railway) for archiving to actually start or stop. The dashboard
PITR enable/disable flow handles the rolling restart automatically; raw
env-var users need one extra restart per node.

### PITR restore creates a new service

Per RFC, PITR restore creates a brand-new Postgres service in the project
(typically a single-node SSL Postgres, not a fresh HA cluster); the
source HA cluster stays online and untouched. The restored service's
volume is populated from the source's snapshot, then booted with
`WAL_RECOVER_FROM_*` pointing at the source's bucket and
`POSTGRES_RECOVERY_TARGET_TIME` set — `archive-get` reads source WAL
during replay. After promote, the restored service has no archive bucket
of its own and runs as plain non-archiving Postgres until the operator
opts in via the standard PITR-enable flow.

Source and restored services therefore never share a write path: there
is no risk of the recovered timeline overwriting the source's ongoing
WAL chain. The previous "mandatory repo-path divergence" guard and the
`.pgbackrest_source_path` sentinel are gone.

For backwards compatibility, when `POSTGRES_RECOVERY_TARGET_TIME` is set
on an HA primary volume directly, `patroni-runner` still creates
`recovery.signal` and writes the matching recovery settings into
`postgresql.auto.conf` before starting Patroni. The `restore_command`
references `--config=/etc/pgbackrest/pgbackrest-recovery-source.conf`
so archive-get during replay reads only the source bucket. Two
filesystem stamps coordinate "exactly once per successful promote":
`.pitr_staging` is written when replay is handed to Postgres,
`.pitr_configured` is written on the boot AFTER Postgres consumes
`recovery.signal` (which it only removes on successful promote). A
failed replay leaves `.pitr_staging` behind WITHOUT `.pitr_configured` —
fix env vars and restart, the next boot re-stages cleanly.

### Image-owned base backups

When `WAL_ARCHIVE_BUCKET` is set, `patroni-runner` spawns a leader-only
backup-watcher task that polls every 60s. Every iteration the watcher
asks the local Patroni REST API (`GET /leader`, 200 = leader, 503 =
replica) — replicas no-op the iteration, so only the current leader
runs backups. After failover the new leader's watcher takes over within
one poll cycle. pgBackRest's stanza locks are the second-line guarantee
against concurrent backups across cluster nodes.

The watcher runs `pgbackrest backup` against the archive bucket when
one of these conditions holds:

1. **Initial backup** — no full has been recorded on this volume.
   Triggers a `--type=full` immediately; pgBackRest brackets the base
   in `pg_backup_start`/`pg_backup_stop` and waits for the closing WAL
   to archive before declaring success, so a broken `archive_command`
   fails the backup loudly instead of producing an unrestorable base.
2. **Gap recovery** — either the archive-push wrapper dropped a segment
   (touches `$PGDATA/.pgbackrest_gap_pending`) or
   `pg_stat_archiver.failed_count` grew since the last full. Once
   archive failures have been quiescent for
   `WAL_BACKUP_GAP_RESOLVED_GRACE_SECONDS` (default 300s), runs a fresh
   full so the PITR window resumes from the new base. The dropped
   segment itself remains unrestorable; everything from the new base
   forward is.
3. **Periodic** — `WAL_BACKUP_FULL_INTERVAL_HOURS` (default 168 h /
   weekly) for fulls, `WAL_BACKUP_DIFF_INTERVAL_HOURS` (default 24 h)
   for differentials. Set either to `0` to disable that schedule.

Every iteration also emits a tiny non-transactional WAL record
(`pg_logical_emit_message`) so `archive_timeout=60` flushes a segment
on idle DBs — without it the picker's "latest restorable" lags
wall-clock by the checkpoint interval (default 5 min) on quiet
services. Disable via `WAL_HEARTBEAT_DISABLED=1`.

State persists at `$PGDATA/.pgbackrest_backup_state` (key=value lines:
`last_full_at`, `last_diff_at`, `last_full_failed_count`). The
bucket-side `pgbackrest --stanza=main info --output=json` is the
canonical source of truth; the local file is a cache that survives
restarts.

**Known gap**: pgBackRest's `archive-push-queue-max` trip drops segments
without going through the archive-push wrapper *and* without
incrementing `failed_count`, so neither gap signal fires. Until log
parsing or LSN-lag detection lands, queue-max-trip gaps are sealed by
the next periodic full rather than promptly.

### Per-cluster archive paths

Each cluster archives under a sub-prefix derived from its
`system_identifier`:
`${WAL_ARCHIVE_PATH}/cluster-<system_identifier>`. The path is
persisted in `$PGDATA/.pgbackrest_repo_path` so the archive-push
wrapper, the backup watcher, `pgbackrest stanza-create`, and out-of-
band invocations (e.g. mono's PITR coverage probe over SSH) all
converge on the same value. `patroni-runner` rewrites the rendered
`/etc/pgbackrest/pgbackrest.conf`'s `repo1-path=` line once
`pg_control` is on disk, so an SSH-driven `pgbackrest info` (which
inherits no shell env from the container) reads the per-cluster path
correctly.

Why per-cluster: a wipe-and-reuse-bucket cycle (operator drops the
data volume, redeploys against the same `WAL_ARCHIVE_BUCKET`) produces
a brand-new `system_identifier` from `initdb`. Without discrimination,
pgBackRest's stanza-create would refuse the new cluster on system-id
mismatch and the new cluster's WAL would never land — silent data loss
for any operator who didn't notice. With per-cluster paths, the new
cluster lands at `cluster-<new_sysid>`, the previous cluster's archive
stays at `cluster-<old_sysid>`, and both histories coexist.

`WAL_RECOVER_FROM_PATH` on a restored cluster must point at the
specific source-side `cluster-<sysid>` sub-prefix the user wants to
restore from — `pgbackrest restore` reads from one path. Backboard
discovers per-cluster sub-prefixes by listing the bucket and surfaces
them as separate "histories" in the restore UI.

### Retention

For PITR-enabled clusters, **`pgbackrest expire` is the sole WAL
retention authority** — no Postgres GUCs, no bucket-side lifecycle
policy. Backup manifests pin the WAL needed to make each backup
restorable; expire releases both together when a backup ages out.
Earlier iterations proposed a bucket-side TTL as a safety net but it's
superfluous: any TTL shorter than expire's horizon would yank WAL out
from under live manifests, and any TTL ≥ that horizon is redundant.

`pgbackrest expire` runs automatically after each `pgbackrest backup`
the watcher invokes, removing fulls/diffs beyond
`WAL_BACKUP_RETENTION_FULL` / `WAL_BACKUP_RETENTION_DIFF`, plus the WAL
their manifests no longer pin. The default retention (full=4, diff=14,
weekly fulls + daily diffs) covers approximately a four-week PITR
window before the oldest full ages out.

## Major version upgrades

A major upgrade is driven from outside this image: the control plane pauses
Patroni failover, stops the leader, runs a one-shot job image carrying both
majors' binaries against the leader's volume, repins the service image, then
rebuilds each replica from the upgraded primary and resumes failover.

While that window is open the job marks the volume with
`.railway-major-upgrade.json` at the **volume root** (PGDATA is replaced
wholesale by `pg_upgrade`, so a marker inside it would not survive). The image
consults that marker in three places, because each of them destroys data if it
ignores it and none of them can see the control plane:

| Guard | Without it |
|-------|-----------|
| Boot refuses while the marker is not `completed` | PGDATA can be absent mid-swap; Patroni treats the member as fresh and bootstraps over it |
| Boot refuses when `PG_VERSION` ≠ the image's major | Postgres only refuses deep into startup, and the incomplete-clone wipe may run first |
| The self-heal watcher stands down | **A Patroni DCS pause does not stop this watcher.** A replica it reinitializes mid-upgrade wipes itself and then cannot clone at all — `pg_basebackup` refuses across majors — so it ends up empty rather than rebuilt |

All three are fail-stop and loud: this image never tries to repair a mid-upgrade
volume, because that state belongs to the upgrade workflow.

The image's own major comes from the installed server tree
(`/usr/lib/postgresql/<major>/`), not from the `PG_MAJOR` env var — the tree
is baked into the image, while a stray user-set `PG_MAJOR` service variable
would otherwise refuse every boot of a perfectly matched data directory. The
env is only a fallback when the tree is ambiguous, and a disagreement is
logged with the filesystem winning.

The self-heal standdown is not a one-shot signal: the
`POSTGRES_HA_SELF_HEAL_UPGRADE_STANDDOWN` event re-emits every 6 hours while
the marker persists, carrying the marker's phase and age. A young marker is a
live upgrade window; one aging past hours with no workflow running is stale
debris (e.g. a boot-time removal that failed, reported separately as a
`COMPONENT_ERROR`) that silently keeps self-heal disabled until it is removed.

One marker phase is the exception to fail-stop, because it exists to make the
replica rebuild possible at all: **`{"phase": "reseed"}`**, written by the HA
workflow onto each replica's volume before it pauses failover. Boot is
*allowed* through both guards; if the on-disk `PG_VERSION` differs from the
image's major the runner wipes pgdata — only under the same safety predicate
as the incomplete-clone wipe (dedicated pgdata subdir + a DISTINCT member
holding the DCS leader lock) — and deletes the marker at wipe time, so Patroni
re-clones the member from the upgraded leader. On a matching major the boot
just sheds the marker (the rollback shape). Streaming replication cannot cross
majors, so without this phase the version-mismatch guard would refuse the
exact boot the reseed depends on. The marker also keeps the self-heal watcher
standing down for the window, which is the production path by which that
standdown actually engages.

### Verified end to end against a real cluster

The whole choreography — pause → leader `pg_upgrade` (16→17) → leader on the
new image → per-replica reseed → resume → switchover — has been exercised
against a live 3-node Patroni 4.1.0 + etcd cluster, and is pinned permanently
by `t_ha_major_upgrade_full_choreography` in `test/e2e-ha.sh` (it runs a real
`pg_upgrade` of the leader's volume using postgres-ssl's job image; CI runs it
as its own job). `t_ha_major_upgrade_back_to_back` additionally chains two
hops (15→16→17) on the same cluster and volumes: hop 2's job runs over hop
1's `completed` marker (history, not state — overwritten at the job's own
commit point), the replicas reseed a second time, and a row written between
the hops survives. The load-bearing facts settled here, each of which the
docs are ambiguous about:

- **Stopping a paused member is an unclean stop by design.** Patroni logs
  `Leader key is not deleted and Postgresql is not stopped due paused state`
  and exits without stopping the postmaster, which the container teardown then
  SIGKILLs. The upgrade job's WAL-replay quiesce is therefore the *normal*
  path for an HA leader, not an edge case. The stale leader key simply expires
  via its DCS lease TTL; paused replicas do not take it.
- **The DCS `initialize` key really does hold the old system identifier, and
  the upgraded leader really is refused — but the observed shape is a wedge,
  not a crash.** Under pause Patroni warns `system ID has changed while in
  paused mode. Patroni will exit when resuming unless system ID is reset:
  <old> != <new>` and then sits at `PAUSE: postgres is not running` forever —
  a paused Patroni never starts a stopped postgres, so the redeployed leader
  stays up with the database down. (Unpaused, that warning becomes the fatal
  "belongs to a different cluster" exit.)
- **The minimal mitigation is two calls, both available from inside a member
  container:** delete *only* `/service/<scope>/initialize` via etcd's HTTP v3
  API (`curl -X POST http://$ETCD_HOST/v3/kv/deleterange` with the base64 key
  — `etcdctl` is not in this image; `curl`, `base64` and `PATRONI_ETCD3_HOSTS`
  are), then `POST /restart` to the leader's Patroni REST. A paused Patroni
  honors the explicit restart, starts postgres, logs `PAUSE: acquired session
  lock as a leader`, and writes a fresh `initialize` key carrying the new
  sysid — while `/config`, **including `pause: true` and every dynamic
  postgresql parameter, survives untouched**.
- **`patronictl remove <scope>` is NOT an acceptable substitute** (despite
  being the mitigation Zalando's procedure describes): verified to delete the
  entire `/service/<scope>/` prefix including `/config`, which destroys the
  pause flag — waking failover mid-window — and every dynamic parameter
  (`archive_mode` etc. on PITR clusters).
- **A paused Patroni will not clone a member on its own.** After the reseed
  boot wipes a cross-major pgdata the member sits at `PAUSE: running with
  empty data directory` indefinitely. `POST /reinitialize` *is* honored under
  pause and performs the basebackup clone from the upgraded leader, so the
  reseed walk can stay inside the paused window — but only with that explicit
  kick per replica.
- **Resume ordering matters and must stay last.** The old-major replicas'
  live (paused) Patronis tolerate the new `initialize` key only *because* the
  cluster is paused; resuming before they are rebuilt would make them exit
  fatally on the sysid mismatch, and the restart that follows on the old image
  would consume the reseed marker (matching-major shape), leaving the later
  repin+redeploy refused by the version-mismatch guard.
- Incidentals: `pg_upgrade --link` succeeds on a Patroni-managed data
  directory (`patroni.dynamic.json`, Patroni-rendered conf and pg_hba are not
  blockers); the job container must receive the service's `PGDATA`
  (`…/data/pgdata`) explicitly, because the job image inherits the official
  base image's `PGDATA=/var/lib/postgresql/data` — the volume root, which the
  job refuses (in production the job runs as a deployment of the service and
  inherits its variables); after the mitigated window a real switchover to a
  reseeded replica completes and bumps the timeline.

## Quick Start

### Deploy to Railway

1. Click "Deploy Template" in Railway marketplace
2. Configure variables (or use defaults):
   - `POSTGRES_USER` - Database username (default: `railway`)
   - `POSTGRES_PASSWORD` - Auto-generated secure password
   - `POSTGRES_DB` - Database name (default: `railway`)
3. Wait for deployment (~2-3 minutes)
4. Connect using the provided `DATABASE_URL`

### Connection Strings

Once deployed, connect to your database via HAProxy:

```bash
# Primary (read-write) - From Railway private network
postgresql://railway:${POSTGRES_PASSWORD}@haproxy.railway.internal:5432/railway

# Replicas (read-only) - For read scaling
postgresql://railway:${POSTGRES_PASSWORD}@haproxy.railway.internal:5433/railway

# From external (via TCP proxy)
postgresql://railway:${POSTGRES_PASSWORD}@${HAPROXY_TCP_PROXY_DOMAIN}/railway
```

HAProxy automatically routes connections:
- **Port 5432**: Routes to current Patroni leader (read-write)
- **Port 5433**: Load-balances across healthy replicas (read-only)

## Configuration

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `POSTGRES_USER` | `railway` | Database superuser |
| `POSTGRES_PASSWORD` | Auto-generated | Superuser password |
| `POSTGRES_DB` | `railway` | Default database name |
| `PATRONI_SCOPE` | `pg-ha-cluster` | Cluster identifier |
| `PATRONI_TTL` | `30` | Leader lease TTL (seconds) |
| `PATRONI_LOOP_WAIT` | `10` | Health check interval |
| `POSTGRES_NODES` | (required for HAProxy) | Node list in format `host:pgport:patroniport,...` |
| `HAPROXY_MAX_CONN` | `1000` | Maximum concurrent connections |
| `HAPROXY_CHECK_INTERVAL` | `3s` | Backend health check interval |

### Scaling HAProxy

HAProxy is stateless and can be scaled horizontally via Railway replicas:

```toml
# In haproxy/railway.toml
[deploy]
numReplicas = 3
```

## Monitoring

### Health Checks

Each service exposes health endpoints:

```bash
# PostgreSQL + Patroni
curl http://postgres-1.railway.internal:8008/health
curl http://postgres-1.railway.internal:8008/cluster  # Full cluster status
curl http://postgres-1.railway.internal:8008/primary  # 200 if primary
curl http://postgres-1.railway.internal:8008/replica  # 200 if replica

# etcd
curl http://etcd-1.railway.internal:2379/health

# HAProxy stats dashboard
curl http://haproxy.railway.internal:8404/stats
```

### Cluster Status

Check Patroni cluster status from any PostgreSQL node:

```bash
curl http://postgres-1.railway.internal:8008/cluster
```

Response:
```json
{
  "members": [
    {
      "name": "postgres-1",
      "role": "leader",
      "state": "running",
      "timeline": 1,
      "lag": 0
    },
    {
      "name": "postgres-2",
      "role": "replica",
      "state": "streaming",
      "timeline": 1,
      "lag": 0
    },
    {
      "name": "postgres-3",
      "role": "replica",
      "state": "streaming",
      "timeline": 1,
      "lag": 0
    }
  ]
}
```

### HAProxy Stats

Access the HAProxy stats dashboard at `http://haproxy.railway.internal:8404/stats` for real-time backend health and connection metrics.

## Failover Behavior

### Automatic Failover (Primary Crashes)

**Timeline**:
```
T+0s   postgres-1 (leader) crashes
T+3s   HAProxy health check fails (3s interval)
T+6s   HAProxy marks backend DOWN (fall 3)
T+8s   Patroni elects new leader via etcd
T+10s  HAProxy routes to new primary
```

**Impact**:
- Existing write connections: Dropped (apps retry)
- Existing read connections: Unaffected (if using :5433)
- New connections: Routed to new primary
- **Total downtime**: ~10 seconds

### Automatic Recovery (Failed Node Returns)

When `postgres-1` recovers:

```
T+0s   postgres-1 restarts
T+3s   Patroni registers with etcd
T+4s   Discovers postgres-2 is leader
T+5s   Rejoins as replica
T+10s  Begins streaming replication
T+12s  HAProxy adds to replica pool
```

**Result**: Original primary rejoins as **replica**, not leader.

### Manual Switchover

To manually switch leaders (zero-downtime):

```bash
# From inside any PostgreSQL container
patronictl -c /etc/patroni/patroni.yml switchover

# Follow prompts to select new leader
```

## Local Development

Test the cluster locally with Docker Compose:

```bash
docker-compose up -d
```

This starts all 7 services on your local machine.

Connect to the cluster:
```bash
# Read-write (primary)
psql postgresql://railway:railway@localhost:5432/railway

# Read-only (replicas)
psql postgresql://railway:railway@localhost:5433/railway
```

View HAProxy stats:
```bash
open http://localhost:8404/stats
```

## Troubleshooting

### Cluster won't start

1. Check etcd is healthy:
   ```bash
   curl http://etcd-1.railway.internal:2379/health
   ```

2. Check Patroni logs for PostgreSQL services:
   ```
   Railway Dashboard → postgres-1 → Logs
   ```

3. Verify private networking is enabled in Railway project settings

### Split-brain (multiple leaders)

This should never happen due to etcd quorum, but if it does:

1. Stop all PostgreSQL services
2. Clear etcd data: `etcdctl del --prefix /service/`
3. Restart services in order: etcd → postgres-1 → postgres-2 → postgres-3

### High replication lag

Check lag from Patroni API:
```bash
curl http://postgres-2.railway.internal:8008/ | jq '.replication[0].lag'
```

If lag is high (>1GB):
- Check network connectivity between nodes
- Increase `wal_keep_size` in patroni configuration
- Consider increasing PostgreSQL resources

### HAProxy connection errors

1. Check HAProxy stats:
   ```bash
   curl http://haproxy.railway.internal:8404/stats
   ```

2. Verify backends are healthy:
   ```bash
   curl http://postgres-1.railway.internal:8008/health
   ```

3. Check HAProxy logs for backend failures

## Cost Estimation

**Resource allocation**:
- 3 PostgreSQL: 2 vCPU, 2GB RAM each + 10GB volume
- 3 etcd: 0.5 vCPU, 512MB RAM each
- 1 HAProxy: 0.5 vCPU, 512MB RAM

**Estimated cost (Railway Pro)**:
- Compute: ~$60-120/month
- Storage: ~$7.50/month (30GB)
- **Total**: ~$70-130/month

**Comparison**:
- AWS RDS Multi-AZ (db.t4g.small): ~$120/month
- GCP Cloud SQL HA: ~$80/month

## Backups

Railway Pro includes automatic volume snapshots:
- Frequency: Daily
- Retention: 6 days (daily), 27 days (weekly), 89 days (monthly)
- Max 10 backups per volume

**Manual backup**:
```bash
pg_dump -h haproxy.railway.internal -U railway railway > backup.sql
```

**Restore from snapshot**:
1. Railway Dashboard → postgres-1 → Volumes → Snapshots
2. Click "Restore" on desired snapshot
3. Create new service from snapshot

## Upgrading PostgreSQL

This template supports PostgreSQL 14, 15, 16, 17, and 18. To upgrade:

1. Create a logical backup:
   ```bash
   pg_dumpall -h haproxy.railway.internal -U railway > cluster_backup.sql
   ```

2. Deploy new template with updated PostgreSQL version

3. Restore data:
   ```bash
   psql -h new-haproxy.railway.internal -U railway < cluster_backup.sql
   ```

4. Update application `DATABASE_URL` to new cluster

5. Delete old cluster after verification

## Security

- All passwords are auto-generated and encrypted at rest
- Private networking isolates cluster from public internet
- SSL enabled by default for PostgreSQL connections
- Recommend enabling Railway's 2FA for project access

## Performance Tuning

### PostgreSQL

Patroni dynamically generates PostgreSQL configuration. Override via environment variables:

```yaml
postgresql:
  parameters:
    shared_buffers: 512MB        # 25% of RAM
    effective_cache_size: 2GB    # 50-75% of RAM
    max_connections: 300         # Increase for high concurrency
    work_mem: 4MB                # Per-query memory
```

### HAProxy

Adjust via environment variables:

```bash
HAPROXY_MAX_CONN=2000           # More concurrent connections
HAPROXY_CHECK_INTERVAL=1s       # Faster failover detection
HAPROXY_TIMEOUT_CLIENT=60m      # Longer idle connections
```

## Docker Images

Pre-built images are published to GitHub Container Registry:

```bash
# PostgreSQL + Patroni (multiple versions)
ghcr.io/railwayapp/postgres-ha/postgres-patroni:18
ghcr.io/railwayapp/postgres-ha/postgres-patroni:17
ghcr.io/railwayapp/postgres-ha/postgres-patroni:16
ghcr.io/railwayapp/postgres-ha/postgres-patroni:15
ghcr.io/railwayapp/postgres-ha/postgres-patroni:14

# etcd
ghcr.io/railwayapp/postgres-ha/etcd:3.5.16

# HAProxy
ghcr.io/railwayapp/postgres-ha/haproxy:3.2
```

## Support

- **Issues**: https://github.com/railwayapp/postgres-ha/issues
- **Documentation**: https://docs.railway.app/
- **Community**: https://discord.gg/railway

## License

MIT License - See LICENSE file for details

## Credits

Built with:
- [PostgreSQL](https://www.postgresql.org/) - World's most advanced open source database
- [Patroni](https://patroni.readthedocs.io/) - Template for PostgreSQL HA with Python
- [etcd](https://etcd.io/) - Distributed reliable key-value store
- [HAProxy](https://www.haproxy.org/) - Reliable, high-performance TCP/HTTP load balancer
- [Railway](https://railway.app/) - Infrastructure, instantly
