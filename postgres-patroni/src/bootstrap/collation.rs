//! Collation version mismatch repair: REINDEX first, then REFRESH.
//!
//! # Why this exists
//!
//! A container image rebuild can move the base distro under a volume — the
//! 2026-08-29 `postgres-ssl:16` digest bump went Debian 12 (glibc 2.36) →
//! Debian 13 (glibc 2.41) because its Dockerfile floated `FROM postgres:16`.
//! glibc's collation order changes between those versions, and every btree
//! index over a collatable column (`text`, `varchar`, `citext`, ...) is
//! physically sorted in the OLD order. Under the new libc, index lookups miss
//! rows that are still in the heap: FK checks fail on rows that exist,
//! `WHERE email = $1` returns nothing, and a customer read it as ~170k lost
//! records. The wrapper then ran `ALTER DATABASE ... REFRESH COLLATION
//! VERSION` — which only rewrites the stamp in `pg_database` and silences
//! Postgres's own WARNING — without reindexing, so the one signal that
//! something was wrong disappeared while the indexes stayed broken.
//!
//! Postgres's documented procedure for a collation library change is:
//! REINDEX every index that depends on the changed collation, THEN refresh
//! the recorded version. This module does exactly that, in that order, and
//! refuses to refresh anything it did not first repair.
//!
//! # Detection
//!
//! Two catalog checks, both against the OS libc this container runs:
//!
//! - database default:
//!   `pg_database.datcollversion IS DISTINCT FROM pg_database_collation_actual_version(oid)`
//! - named libc collations (`COLLATE "en_US"` columns):
//!   `pg_collation.collversion IS DISTINCT FROM pg_collation_actual_version(oid)`
//!   for `collprovider = 'c'`
//!
//! The catalogs ARE the state: once the refresh has run, neither query
//! returns a row, so this whole procedure is idempotent and needs no marker
//! file. That matters in an HA cluster (see below) because a marker in the
//! data dir would not travel to a promoted replica, but the catalog does.
//!
//! Scope: libc (`collprovider = 'c'`) only. `C`/`POSIX`/`ucs_basic` carry no
//! version (both sides NULL, never a mismatch). ICU (`'i'`) and the builtin
//! provider (`'b'`) are out of scope here — an ICU mismatch is logged loudly
//! and deliberately NOT refreshed, so Postgres's per-connection WARNING stays
//! visible until someone reindexes it by hand.
//!
//! # HA / Patroni
//!
//! Everything here runs on the LEADER only. `REINDEX` writes new index
//! relfiles and the swap commits through WAL, so replicas receive the
//! rebuilt indexes by streaming; `ALTER DATABASE` is DDL and fails read-only
//! on a standby anyway. Before the run, and again before every single
//! `REINDEX`, the leader gate re-checks `pg_is_in_recovery() = false` AND
//! (under Patroni) `GET /leader` = 200, no `scheduled_switchover` in
//! `GET /cluster`, and the cluster not paused. A node that loses the lock
//! mid-run stops at the next index; Patroni's demotion restarts its postgres
//! anyway, which kills the in-flight REINDEX session.
//!
//! Failover mid-way: a promoted replica sees the leader's catalog state
//! through WAL — databases already refreshed no longer match and are
//! skipped; a database whose reindex was interrupted still matches (its
//! stamp was never refreshed, because refresh only follows a fully
//! successful reindex), so the new leader redoes that database from the
//! top. That repeats some work but never leaves a refreshed-but-unrepaired
//! database behind, which is the invariant that matters. The same catalog
//! gate is why two nodes can never both run it: only one holds the leader
//! lock, and the loser's session dies with its demotion.
//!
//! Rolling image update (replicas rebuilt on the new glibc first, then the
//! leader): a replica on glibc 2.41 streaming from a leader on 2.36 holds
//! indexes sorted by 2.36 rules and reads them with 2.41 rules — its
//! read-only queries can miss rows for the duration of the rollout, and
//! nothing on the replica can fix that (a standby cannot REINDEX). The fix
//! arrives when the LEADER is redeployed on 2.41, boots, detects the
//! mismatch, and reindexes: the rebuilt indexes replicate to every standby.
//! Conversely, any straggler replica still on 2.36 after that point reads
//! 2.41-ordered indexes with 2.36 rules until its own redeploy lands. Both
//! windows are inherent to a libc change under a streaming cluster; the
//! image pins its base distro per major precisely so this path is only ever
//! taken deliberately, never by a same-tag digest bump.
//!
//! # Concurrency and locks
//!
//! User indexes are rebuilt with `REINDEX INDEX CONCURRENTLY` (no write
//! lock on the table). System-catalog indexes and exclusion-constraint
//! indexes cannot be rebuilt concurrently and get a plain `REINDEX INDEX`
//! (brief SHARE lock; catalog indexes are tiny). Invalid `*_ccnew*` /
//! `*_ccold*` leftovers of an earlier interrupted concurrent reindex are
//! dropped first, per the Postgres manual's recovery procedure for exactly
//! that situation. Any REINDEX failure aborts that database WITHOUT
//! refreshing it, so the next boot or promotion retries it.
//!
//! Kill switch: `COLLATION_REINDEX_DISABLED=1` skips the whole procedure and
//! leaves Postgres's mismatch WARNING in place. There is intentionally no
//! "refresh without reindex" mode.

use super::{read_credentials, run_psql, run_psql_in_db};
use crate::pgdata;
use anyhow::{anyhow, Context, Result};
use std::fs;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Set to `1`/`true` to disable the reindex+refresh procedure entirely.
pub const KILL_SWITCH_ENV: &str = "COLLATION_REINDEX_DISABLED";

const PATRONI_LEADER_URL: &str = "http://localhost:8008/leader";
const PATRONI_CLUSTER_URL: &str = "http://localhost:8008/cluster";
const PATRONI_SELF_URL: &str = "http://localhost:8008/patroni";

/// Every connectable database (template1 included: it is connectable, its
/// catalog indexes are reindexed in a blink, and refreshing it stops the
/// WARNING on `CREATE DATABASE`). template0 is never connectable.
const LIST_DATABASES_SQL: &str = "COPY (SELECT datname FROM pg_database \
     WHERE datallowconn AND datname <> 'template0' ORDER BY datname) TO STDOUT";

/// Database-default collation census, run once from the maintenance
/// database: name, locale provider, stored version, actual version — for
/// every database whose stored stamp differs from what this container's
/// libc reports. Same predicate as the postgres-ssl sibling.
const DATABASE_MISMATCH_SQL: &str = "COPY (SELECT d.datname, d.datlocprovider, \
     coalesce(d.datcollversion, ''), coalesce(pg_database_collation_actual_version(d.oid), '') \
     FROM pg_database d \
     WHERE d.datallowconn AND d.datname <> 'template0' \
       AND d.datcollversion IS DISTINCT FROM pg_database_collation_actual_version(d.oid) \
     ORDER BY d.datname) TO STDOUT WITH (DELIMITER E'\\t')";

/// Named libc collations whose stored version differs from the installed
/// libc. pg_collation is per-database, so this runs inside each one.
const LIBC_COLLATION_MISMATCH_SQL: &str = "COPY (SELECT format('%I.%I', n.nspname, c.collname), \
     coalesce(c.collversion, ''), coalesce(pg_collation_actual_version(c.oid), '') \
     FROM pg_collation c JOIN pg_namespace n ON n.oid = c.collnamespace \
     WHERE c.collprovider = 'c' \
       AND c.collversion IS DISTINCT FROM pg_collation_actual_version(c.oid) \
     ORDER BY 1) TO STDOUT WITH (DELIMITER E'\\t')";

/// Count of indexes that depend on a mismatched ICU collation (default or
/// named). Reported, never repaired here — see the module doc.
const ICU_AFFECTED_INDEX_COUNT_SQL: &str = "COPY (WITH affected AS ( \
       SELECT 100::oid AS colloid \
       WHERE EXISTS (SELECT 1 FROM pg_database d WHERE d.datname = current_database() \
                     AND d.datlocprovider = 'i' \
                     AND d.datcollversion IS DISTINCT FROM pg_database_collation_actual_version(d.oid)) \
       UNION \
       SELECT c.oid FROM pg_collation c \
       WHERE c.collprovider = 'i' \
         AND c.collversion IS DISTINCT FROM pg_collation_actual_version(c.oid)) \
     SELECT count(*) FROM pg_index i JOIN pg_class ic ON ic.oid = i.indexrelid \
     WHERE ic.relkind = 'i' \
       AND EXISTS (SELECT 1 FROM unnest(i.indcollation::oid[]) AS u(colloid) \
                   WHERE u.colloid IN (SELECT colloid FROM affected))) TO STDOUT";

/// Invalid leftovers of an interrupted `REINDEX ... CONCURRENTLY`. The
/// manual: a `_ccnew` suffix is the transient index that never finished —
/// drop it and reindex again; a `_ccold` suffix is the original that could
/// not be dropped after a successful swap — just drop it.
const CC_LEFTOVERS_SQL: &str = "COPY (SELECT format('%I.%I', n.nspname, ic.relname) \
     FROM pg_index i JOIN pg_class ic ON ic.oid = i.indexrelid \
     JOIN pg_namespace n ON n.oid = ic.relnamespace \
     WHERE NOT i.indisvalid AND ic.relkind = 'i' \
       AND ic.relname ~ '_cc(new|old)[0-9]*$' \
     ORDER BY 1) TO STDOUT";

/// Indexes to rebuild in the current database, smallest first (fast
/// feedback in the log; the biggest one runs last), catalogs before user
/// tables. Selected by `pg_index.indcollation`: a column that is not
/// collatable stores 0 there, an explicit `COLLATE "C"` stores C's oid
/// (never in the affected set — it has no version), and a default-collation
/// text column stores 100 (`pg_catalog."default"`). `{with_default}` is
/// spliced as SQL `true`/`false`: whether the database's own default
/// collation is a mismatched libc one. Partitioned parents (`relkind 'I'`)
/// are skipped — their leaves are listed themselves. Other sessions' temp
/// indexes are skipped. Invalid indexes are skipped (a REINDEX would
/// validate them behind their owner's back; the `_cc*` leftovers are
/// handled separately above).
const AFFECTED_INDEXES_SQL_TEMPLATE: &str = "COPY (WITH affected AS ( \
       SELECT 100::oid AS colloid WHERE {with_default} \
       UNION \
       SELECT c.oid FROM pg_collation c \
       WHERE c.collprovider = 'c' \
         AND c.collversion IS DISTINCT FROM pg_collation_actual_version(c.oid)) \
     SELECT format('%I.%I', n.nspname, ic.relname), \
            CASE WHEN n.nspname = 'pg_catalog' OR i.indisexclusion THEN 'plain' ELSE 'concurrent' END, \
            pg_relation_size(ic.oid) \
     FROM pg_index i \
     JOIN pg_class ic ON ic.oid = i.indexrelid \
     JOIN pg_namespace n ON n.oid = ic.relnamespace \
     WHERE ic.relkind = 'i' \
       AND ic.relpersistence <> 't' \
       AND i.indisvalid \
       AND EXISTS (SELECT 1 FROM unnest(i.indcollation::oid[]) AS u(colloid) \
                   WHERE u.colloid IN (SELECT colloid FROM affected)) \
     ORDER BY (n.nspname = 'pg_catalog') DESC, pg_relation_size(ic.oid), 1) \
     TO STDOUT WITH (DELIMITER E'\\t')";

/// One row of `DATABASE_MISMATCH_SQL`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseMismatch {
    pub datname: String,
    /// `c` libc, `i` ICU, `b` builtin (PG17+).
    pub provider: char,
    pub stored: String,
    pub actual: String,
}

/// One row of `AFFECTED_INDEXES_SQL_TEMPLATE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AffectedIndex {
    /// `schema.index`, already identifier-quoted by `format('%I.%I')`.
    pub qualified_name: String,
    pub concurrent: bool,
    pub size_bytes: u64,
}

/// Why the procedure did not run (all of these are logged, none is fatal).
#[derive(Debug, PartialEq, Eq)]
pub enum Skip {
    KillSwitch,
    NotPrimary,
    NotPatroniLeader,
    SwitchoverScheduled,
    ClusterPaused,
}

/// Repair collation version mismatches on all connectable databases:
/// REINDEX the affected indexes, then REFRESH the recorded versions —
/// leader only, idempotent, safe across failovers (see the module doc).
///
/// No-op on PG < 15 (`datcollversion` and `REFRESH COLLATION VERSION` were
/// introduced in PG 15) and when PG_VERSION can't be read (pre-initdb).
/// Every failure is logged and swallowed: this must never take a node down.
pub fn refresh_collation_versions() {
    let pg_version_file = format!("{}/PG_VERSION", pgdata());
    let pg_major: u32 = match fs::read_to_string(&pg_version_file) {
        Ok(v) => v.trim().parse().unwrap_or(0),
        Err(_) => return,
    };
    if pg_major < 15 {
        return;
    }

    if kill_switch_engaged() {
        tracing::warn!(
            "collation-refresh: {KILL_SWITCH_ENV} is set — skipping; any collation \
             version mismatch stays unrepaired and Postgres keeps warning about it"
        );
        return;
    }

    let superuser = match read_credentials() {
        Ok(c) => c.superuser,
        Err(e) => {
            tracing::warn!(error = %e, "collation-refresh: could not read credentials");
            return;
        }
    };

    // Callers only get here when they believe this node is (becoming) the
    // primary: patroni-runner after pg_is_in_recovery() flipped, and the
    // on_role_change callback right after promotion. Patroni's REST view
    // can lag that by a moment (a 503 on /leader that clears within
    // seconds), and the callback fires exactly once — so give the
    // transient reasons a bounded grace instead of skipping a repair that
    // would then wait for the next boot. Switchover/pause are not
    // transient in that sense and return immediately.
    if let Err(skip) = leader_gate_with_grace(&superuser, Duration::from_secs(60)) {
        tracing::info!(reason = ?skip, "collation-refresh: skipped (leader gate)");
        return;
    }

    let databases = match run_psql(&superuser, LIST_DATABASES_SQL) {
        Ok(out) => out,
        Err(e) => {
            tracing::warn!(error = %e, "collation-refresh: could not list databases");
            return;
        }
    };
    let databases: Vec<String> = databases
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();

    let db_mismatches = match run_psql(&superuser, DATABASE_MISMATCH_SQL) {
        Ok(out) => parse_database_mismatches(&out),
        Err(e) => {
            tracing::warn!(error = %e, "collation-refresh: database collation census failed");
            return;
        }
    };

    let mut repaired = 0usize;
    let mut failed = 0usize;
    for db in &databases {
        let default_mismatch = db_mismatches.iter().find(|m| &m.datname == db);
        match repair_database(&superuser, db, default_mismatch) {
            Ok(true) => repaired += 1,
            Ok(false) => {}
            Err(e) => {
                failed += 1;
                tracing::error!(
                    database = %db,
                    error = %e,
                    "collation-refresh: database left UNREFRESHED — its collation version \
                     mismatch is still recorded and this will be retried on the next boot \
                     or promotion"
                );
            }
        }
    }

    if failed == 0 {
        tracing::info!(
            databases = databases.len(),
            repaired,
            "collation-refresh: completed for all databases"
        );
    } else {
        tracing::warn!(
            databases = databases.len(),
            repaired,
            failed,
            "collation-refresh: completed with failures"
        );
    }
}

/// Repair one database. Returns `Ok(true)` when something was reindexed and
/// refreshed, `Ok(false)` when nothing was mismatched (or only ICU was),
/// `Err` when a REINDEX failed — in which case NOTHING was refreshed.
fn repair_database(
    superuser: &str,
    db: &str,
    default_mismatch: Option<&DatabaseMismatch>,
) -> Result<bool> {
    // Named libc collations that moved. pg_collation is per-database.
    let libc_collations = run_psql_in_db(superuser, db, LIBC_COLLATION_MISMATCH_SQL)
        .context("libc collation census")?;
    let libc_collations: Vec<(String, String, String)> = libc_collations
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            let mut f = l.split('\t');
            Some((
                f.next()?.to_string(),
                f.next()?.to_string(),
                f.next()?.to_string(),
            ))
        })
        .collect();

    // ICU: report, never silence.
    if let Ok(out) = run_psql_in_db(superuser, db, ICU_AFFECTED_INDEX_COUNT_SQL) {
        let n: u64 = out.trim().parse().unwrap_or(0);
        if n > 0 {
            tracing::warn!(
                database = %db,
                affected_indexes = n,
                "collation-refresh: ICU collation version changed under {n} index(es); \
                 NOT handled by this image — REINDEX them and run \
                 ALTER COLLATION ... REFRESH VERSION / ALTER DATABASE ... REFRESH COLLATION VERSION \
                 by hand. Postgres keeps warning until then."
            );
        }
    }

    let default_is_libc_mismatch = match default_mismatch {
        Some(m) if m.provider == 'c' => {
            tracing::warn!(
                database = %db,
                stored = %m.stored,
                actual = %m.actual,
                "collation-refresh: database default (libc) collation version changed — \
                 indexes on collatable columns are unreliable until reindexed"
            );
            true
        }
        Some(m) => {
            tracing::warn!(
                database = %db,
                provider = %m.provider,
                stored = %m.stored,
                actual = %m.actual,
                "collation-refresh: database default collation version changed under a \
                 non-libc provider; NOT refreshed by this image (see the ICU note above)"
            );
            false
        }
        None => false,
    };

    if !default_is_libc_mismatch && libc_collations.is_empty() {
        return Ok(false);
    }

    for (name, stored, actual) in &libc_collations {
        tracing::info!(database = %db, collation = %name, stored = %stored, actual = %actual,
            "collation-refresh: named libc collation version changed");
    }

    // Re-check right before the first write of this database: the census
    // above may have taken a while on a cluster with many databases.
    leader_gate(superuser).map_err(|s| anyhow!("leader gate: {s:?}"))?;

    // Interrupted-concurrent-reindex leftovers first (manual's procedure).
    let leftovers = run_psql_in_db(superuser, db, CC_LEFTOVERS_SQL).context("cc leftover scan")?;
    for idx in leftovers.lines().map(str::trim).filter(|l| !l.is_empty()) {
        run_psql_in_db(superuser, db, &format!("DROP INDEX {idx}"))
            .with_context(|| format!("drop invalid concurrent-reindex leftover {idx}"))?;
        tracing::warn!(database = %db, index = %idx,
            "collation-refresh: dropped invalid leftover of an interrupted REINDEX CONCURRENTLY");
    }

    let sql = affected_indexes_sql(default_is_libc_mismatch);
    let indexes = parse_affected_indexes(
        &run_psql_in_db(superuser, db, &sql).context("affected index census")?,
    );
    let total_bytes: u64 = indexes.iter().map(|i| i.size_bytes).sum();
    tracing::info!(
        database = %db,
        indexes = indexes.len(),
        total_bytes,
        "collation-refresh: reindexing indexes that depend on a changed libc collation \
         (BEFORE refreshing the recorded version)"
    );

    for (n, idx) in indexes.iter().enumerate() {
        // A switchover or a lost leader lock between two indexes stops the
        // run here; nothing below has been refreshed, so the next leader
        // starts this database over.
        leader_gate(superuser)
            .map_err(|s| anyhow!("leader gate before {}: {s:?}", idx.qualified_name))?;
        let stmt = if idx.concurrent {
            format!("REINDEX INDEX CONCURRENTLY {}", idx.qualified_name)
        } else {
            format!("REINDEX INDEX {}", idx.qualified_name)
        };
        let started = Instant::now();
        run_psql_in_db(superuser, db, &stmt).with_context(|| stmt.clone())?;
        tracing::info!(
            database = %db,
            index = %idx.qualified_name,
            concurrent = idx.concurrent,
            size_bytes = idx.size_bytes,
            elapsed_ms = started.elapsed().as_millis() as u64,
            progress = format!("{}/{}", n + 1, indexes.len()),
            "collation-refresh: reindexed"
        );
    }

    // Only now — every dependent index is consistent with the running libc.
    for (name, _, _) in &libc_collations {
        run_psql_in_db(
            superuser,
            db,
            &format!("ALTER COLLATION {name} REFRESH VERSION"),
        )
        .with_context(|| format!("ALTER COLLATION {name} REFRESH VERSION"))?;
    }
    if !libc_collations.is_empty() {
        tracing::info!(database = %db, collations = libc_collations.len(),
            "collation-refresh: refreshed named libc collation versions");
    }
    if default_is_libc_mismatch {
        let stmt = format!(
            "ALTER DATABASE {} REFRESH COLLATION VERSION",
            super::quote_ident(db)
        );
        run_psql(superuser, &stmt).with_context(|| stmt.clone())?;
        tracing::info!(database = %db, "collation-refresh: refreshed database collation version");
    }
    Ok(true)
}

fn kill_switch_engaged() -> bool {
    matches!(
        std::env::var(KILL_SWITCH_ENV).as_deref().map(str::trim),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes")
    )
}

/// May this node run DDL/REINDEX right now? `pg_is_in_recovery()` must be
/// false; under Patroni the local REST API must also say this member holds
/// the leader lock, no switchover is scheduled and the cluster is not
/// paused (maintenance mode — a human is choreographing something, e.g. a
/// major upgrade; stay out of the way, the post-switchover promotion will
/// call us again).
fn leader_gate(superuser: &str) -> std::result::Result<(), Skip> {
    // run_psql keeps psql's table formatting; COPY gives us only the value,
    // like the catalog queries below, so a writable primary reads as "f".
    let out = run_psql(superuser, "COPY (SELECT pg_is_in_recovery()) TO STDOUT")
        .map_err(|_| Skip::NotPrimary)?;
    if out.trim() != "f" {
        return Err(Skip::NotPrimary);
    }
    // PATRONI_ENABLED, or the rendered Patroni config on disk: the
    // on_role_change callback runs in Patroni's (trimmed) environment, and
    // the config file is the same evidence that binary already relies on.
    let under_patroni =
        crate::is_patroni_enabled() || std::path::Path::new(super::PATRONI_CONFIG).exists();
    if !under_patroni {
        return Ok(());
    }
    if curl_status(PATRONI_LEADER_URL).as_deref() != Some("200") {
        return Err(Skip::NotPatroniLeader);
    }
    if let Some(cluster) = curl_body(PATRONI_CLUSTER_URL) {
        if cluster_has_scheduled_switchover(&cluster) {
            return Err(Skip::SwitchoverScheduled);
        }
    }
    if let Some(me) = curl_body(PATRONI_SELF_URL) {
        if patroni_is_paused(&me) {
            return Err(Skip::ClusterPaused);
        }
    }
    Ok(())
}

/// `leader_gate`, retried every 2s for up to `budget` while the reason is
/// one that a promotion in progress makes transient (`NotPrimary`,
/// `NotPatroniLeader`). Any other reason is returned at once.
fn leader_gate_with_grace(superuser: &str, budget: Duration) -> std::result::Result<(), Skip> {
    let deadline = Instant::now() + budget;
    loop {
        match leader_gate(superuser) {
            Ok(()) => return Ok(()),
            Err(skip @ (Skip::NotPrimary | Skip::NotPatroniLeader))
                if Instant::now() < deadline =>
            {
                tracing::debug!(reason = ?skip, "collation-refresh: leader gate not open yet, waiting");
                std::thread::sleep(Duration::from_secs(2));
            }
            Err(skip) => return Err(skip),
        }
    }
}

/// GET endpoints of the local Patroni REST API are unauthenticated (the
/// image's own HEALTHCHECK relies on that). `curl` rather than reqwest
/// because this runs both from a plain synchronous binary (on-role-change)
/// and from inside patroni-runner's tokio runtime, where a blocking HTTP
/// client would have to be kept off the async workers.
fn curl_status(url: &str) -> Option<String> {
    let out = Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "--max-time",
            "5",
            url,
        ])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn curl_body(url: &str) -> Option<String> {
    let out = Command::new("curl")
        .args(["-sf", "--max-time", "5", url])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

/// `GET /cluster` carries a top-level `scheduled_switchover` object while
/// one is pending.
pub fn cluster_has_scheduled_switchover(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("scheduled_switchover").cloned())
        .map(|s| !s.is_null())
        .unwrap_or(false)
}

/// `GET /patroni` carries `"pause": true` while the cluster is in
/// maintenance mode.
pub fn patroni_is_paused(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("pause").and_then(|p| p.as_bool()))
        .unwrap_or(false)
}

/// Splice the database-default decision into the affected-index query.
pub fn affected_indexes_sql(with_default: bool) -> String {
    AFFECTED_INDEXES_SQL_TEMPLATE.replace(
        "{with_default}",
        if with_default { "true" } else { "false" },
    )
}

/// Parse `DATABASE_MISMATCH_SQL` COPY output (tab-separated).
pub fn parse_database_mismatches(out: &str) -> Vec<DatabaseMismatch> {
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            let mut f = l.split('\t');
            let datname = f.next()?.to_string();
            let provider = f.next()?.chars().next().unwrap_or('?');
            let stored = f.next()?.to_string();
            let actual = f.next()?.to_string();
            Some(DatabaseMismatch {
                datname,
                provider,
                stored,
                actual,
            })
        })
        .collect()
}

/// Parse `AFFECTED_INDEXES_SQL_TEMPLATE` COPY output (tab-separated).
pub fn parse_affected_indexes(out: &str) -> Vec<AffectedIndex> {
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            let mut f = l.split('\t');
            let qualified_name = f.next()?.to_string();
            let concurrent = f.next()? == "concurrent";
            let size_bytes = f.next()?.trim().parse().unwrap_or(0);
            Some(AffectedIndex {
                qualified_name,
                concurrent,
                size_bytes,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_census_parses_provider_and_versions() {
        let out = "railway\tc\t2.36\t2.41\nicudb\ti\t153.112\t\n";
        let rows = parse_database_mismatches(out);
        assert_eq!(
            rows,
            vec![
                DatabaseMismatch {
                    datname: "railway".into(),
                    provider: 'c',
                    stored: "2.36".into(),
                    actual: "2.41".into(),
                },
                DatabaseMismatch {
                    datname: "icudb".into(),
                    provider: 'i',
                    stored: "153.112".into(),
                    actual: "".into(),
                },
            ]
        );
    }

    #[test]
    fn affected_index_census_orders_and_flags_plain_vs_concurrent() {
        let out = "pg_catalog.pg_seclabel_object_index\tplain\t8192\n\
                   public.users_email_key\tconcurrent\t1048576\n\
                   public.\"Weird Name\"\tconcurrent\t16384\n";
        let rows = parse_affected_indexes(out);
        assert_eq!(rows.len(), 3);
        assert!(!rows[0].concurrent);
        assert_eq!(
            rows[0].qualified_name,
            "pg_catalog.pg_seclabel_object_index"
        );
        assert!(rows[1].concurrent);
        assert_eq!(rows[1].size_bytes, 1_048_576);
        assert_eq!(rows[2].qualified_name, "public.\"Weird Name\"");
    }

    #[test]
    fn affected_index_sql_splices_the_default_decision() {
        assert!(affected_indexes_sql(true).contains("SELECT 100::oid AS colloid WHERE true"));
        assert!(affected_indexes_sql(false).contains("SELECT 100::oid AS colloid WHERE false"));
        assert!(!affected_indexes_sql(true).contains("{with_default}"));
    }

    #[test]
    fn reindex_selection_excludes_icu_and_unversioned_collations_by_construction() {
        // The affected set is built only from collprovider = 'c' rows with a
        // version delta plus (optionally) the default collation; C/POSIX
        // have NULL on both sides and never enter it, ICU is a different
        // provider letter.
        let sql = affected_indexes_sql(false);
        assert!(sql.contains("c.collprovider = 'c'"));
        assert!(!sql.contains("collprovider = 'i'"));
    }

    #[test]
    fn switchover_detection_reads_patroni_cluster_json() {
        assert!(cluster_has_scheduled_switchover(
            r#"{"members":[],"scheduled_switchover":{"at":"2026-09-20T10:00:00+00:00","from":"a","to":"b"}}"#
        ));
        assert!(!cluster_has_scheduled_switchover(r#"{"members":[]}"#));
        assert!(!cluster_has_scheduled_switchover(
            r#"{"members":[],"scheduled_switchover":null}"#
        ));
        assert!(!cluster_has_scheduled_switchover("not json"));
    }

    #[test]
    fn pause_detection_reads_patroni_self_json() {
        assert!(patroni_is_paused(
            r#"{"state":"running","role":"master","pause":true}"#
        ));
        assert!(!patroni_is_paused(r#"{"state":"running","role":"master"}"#));
        assert!(!patroni_is_paused("garbage"));
    }

    #[test]
    fn kill_switch_accepts_truthy_values_only() {
        let _guard = crate::patroni::rest::test_support::ENV_LOCK.lock().unwrap();
        for (v, expect) in [
            ("1", true),
            ("true", true),
            ("yes", true),
            ("0", false),
            ("", false),
        ] {
            std::env::set_var(KILL_SWITCH_ENV, v);
            assert_eq!(kill_switch_engaged(), expect, "value {v:?}");
        }
        std::env::remove_var(KILL_SWITCH_ENV);
        assert!(!kill_switch_engaged());
    }
}
