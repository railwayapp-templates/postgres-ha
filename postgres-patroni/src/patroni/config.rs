//! Patroni configuration from environment variables

use crate::{pgdata, ssl_dir};
use anyhow::Result;
use common::ConfigExt;
use std::env;
use tracing::{info, warn};

/// Configuration for Patroni runner
pub struct Config {
    pub scope: String,
    pub name: String,
    pub connect_address: String,
    pub etcd_hosts: String,
    pub superuser: String,
    pub superuser_pass: String,
    pub repl_user: String,
    pub repl_pass: String,
    pub app_user: String,
    pub app_pass: String,
    pub app_db: String,
    pub data_dir: String,
    pub certs_dir: String,
    pub ttl: String,
    pub loop_wait: String,
    pub retry_timeout: String,
    pub health_check_interval: u64,
    pub health_check_timeout: u64,
    pub max_failures: u32,
    pub startup_grace_period: u64,
    /// Maximum time to wait for Patroni to become healthy during startup.
    /// If exceeded, we exit(1) to trigger container restart and recovery.
    /// Must be >= startup_grace_period. Default: 1800 seconds (30 minutes).
    /// The startup loop is progress-gated (stall timer only advances while the
    /// volume shows zero growth), so this only fires on 30 min of genuine zero
    /// progress — not on a large clone that is making steady forward progress.
    pub max_startup_timeout: u64,
    pub adopt_existing_data: bool,
    /// If true and node has no existing data, wait for cluster leader in etcd
    /// before starting Patroni. Used during HA conversion to prevent replicas
    /// from racing with the primary for leadership.
    pub wait_for_leader: bool,
    /// If true, enable synchronous replication mode. Ensures at least one
    /// replica has received the data before a write is acknowledged.
    pub synchronous_mode: bool,
    /// This cluster's own archive bucket. Tool-agnostic name read from
    /// `WAL_ARCHIVE_BUCKET`; patroni-runner translates it (and the matching
    /// `WAL_ARCHIVE_KEY` / `_SECRET` / `_REGION` / `_ENDPOINT` / `_PATH`)
    /// into pgBackRest's native `PGBACKREST_REPO1_S3_*` (or `_REPO2_*` in
    /// dual-repo mode) so pgBackRest reads them natively. When set, Patroni
    /// DCS gets `archive_mode=on` + the archive-push wrapper as
    /// `archive_command` + `archive_timeout`, and patroni-runner renders
    /// `/etc/pgbackrest/pgbackrest.conf`. Only the current Patroni leader
    /// fires archive_command; standbys carry the same config + binary so
    /// promotion instantly enables archiving. pgBackRest runs in async mode
    /// with `archive-push-queue-max=5GiB` — when the queue trips, WAL is
    /// dropped and Postgres keeps running rather than halting on a full
    /// `pg_wal`. When unset, Patroni config is generated as it always was.
    pub wal_archive_bucket: Option<String>,
    /// Source cluster's bucket on a PITR-restored volume, read by
    /// `archive-get` during recovery only. Translated by patroni-runner from
    /// `WAL_RECOVER_FROM_*` to pgBackRest's `PGBACKREST_REPO1_S3_*`. Under
    /// the new-service restore design (per RFC) HA restore creates a fresh
    /// single-node service rather than restoring in place, so this is rare
    /// in HA but kept for symmetry with postgres-ssl.
    pub wal_recover_from_bucket: Option<String>,
    /// Target timestamp for point-in-time recovery (ISO 8601). When set on a
    /// restored volume, the runner stages `recovery.signal` + recovery
    /// settings before Patroni starts Postgres.
    pub pitr_target_time: Option<String>,
    /// Target transaction ID for point-in-time recovery. When set, takes
    /// precedence over `pitr_target_time` because it's the only target type
    /// postgres can match exactly on an idle source. `recovery_target_time`
    /// requires postgres to observe a WAL record with timestamp > target
    /// before declaring "target reached" and firing
    /// `recovery_target_action=promote`; on an idle DB no such record exists,
    /// so recovery FATALs with "recovery ended before configured recovery
    /// target was reached" and the cluster either crash-loops the FATAL or
    /// hangs in hot_standby read-only mode. `recovery_target_xid` matches an
    /// exact transaction ID — applying the target xid's commit is
    /// unambiguously "target reached." The picker (mono's
    /// createServiceFromPITR mutation) sets `_XID` when it clamped target
    /// down to `lastCommittedTxnAt`; leaves it unset for arbitrary historical
    /// times. Mirrors postgres-ssl PR #63.
    pub pitr_target_xid: Option<String>,
    /// `archive_timeout` written into Patroni's bootstrap.dcs and asserted by
    /// the DCS reconciler when archiving is enabled. Default 60s. Operators
    /// raise it on idle DBs to cut S3 cost or lower it for tighter RPO.
    pub archive_timeout_secs: i64,
    /// `--max-rate` for pg_basebackup during replica creation, so a re-seed
    /// can never monopolize the leader's volume. Validated
    /// `POSTGRES_BASEBACKUP_MAX_RATE` override or the `20M` default.
    pub basebackup_max_rate: String,
    /// `max_slot_wal_keep_size` — the ceiling on WAL the leader retains for a
    /// lagging member's replication slot. Rendered as a PG size string, or
    /// `-1` (unlimited, PG's default) when the volume can't be measured.
    /// Sized off this node's own volume; validated
    /// `POSTGRES_MAX_SLOT_WAL_KEEP_SIZE` override wins when set.
    pub max_slot_wal_keep_size: String,
}

/// Percent of the TOTAL volume a slot may pin, regardless of how empty the disk
/// looks right now. The free-space bound below is the one that actually
/// protects the primary, but it's measured once at startup: a node that has just
/// re-cloned has a small data dir and momentarily abundant free space, and the
/// data grows afterwards while this value stays put. Bounding against total as
/// well keeps that case honest.
const SLOT_KEEP_TOTAL_PCT: u64 = 25;

/// Percent of CURRENT FREE space a slot may pin. This is the half that keeps the
/// primary alive: what matters isn't how big the disk is, it's how much of it is
/// still empty. The remainder absorbs data growth, `max_wal_size`, the pgBackRest
/// spool (itself capped at 5 GiB by `compute_volume_thresholds`) and recovery
/// temp files.
///
/// Tighter when archiving is on, because the replica then has a cheap way back —
/// `restore_command` pulls the missing WAL from S3, and a re-seed streams from
/// S3 rather than off the leader (#78) — so we can bias hard toward primary
/// survival. Looser when it isn't, because invalidation there forces a
/// throttled `pg_basebackup` re-clone off the live leader; still bounded,
/// because an unbounded slot kills the primary outright.
const SLOT_KEEP_FREE_PCT_ARCHIVING: u64 = 50;
const SLOT_KEEP_FREE_PCT_NO_ARCHIVE: u64 = 75;

/// Keep the cap functional: 4 × 16 MiB WAL segments. A cap near zero invalidates
/// slots on routine churn (a replica restart, a brief disconnect) and re-clones
/// forever. Deliberately small, and never allowed to exceed the total-based
/// bound — a floor larger than the volume is not a cap at all, it's the
/// unbounded default wearing a hat. (An earlier 1 GiB floor did exactly that on
/// the fleet's 500 MB volumes: 1 GiB > the whole disk, so the slot could never
/// be invalidated and the leader could still fill up.)
const SLOT_KEEP_FLOOR_MIB: u64 = 64;

/// Resolve `max_slot_wal_keep_size` from this node's volume size.
///
/// `use_slots: true` means every member holds a physical replication slot on
/// the leader, and PostgreSQL's default `max_slot_wal_keep_size = -1` lets a
/// slot pin WAL **without bound**. A replica that replays slower than the
/// leader writes therefore grows the leader's `pg_wal` until the volume is
/// full, at which point the leader takes `PANIC: could not write to file
/// "pg_wal/xlogtemp.N": No space left on device`, crash-restarts, loses its
/// etcd lock and bumps the timeline — a sick *replica* killing the *primary*,
/// which inverts the entire point of HA. Confirmed live 2026-07-10: a 2 TB
/// leader volume went 1083 GB → 1985 GB in 13 h and PANICked.
///
/// Capping trades that outage for a bounded, recoverable one: past the cap
/// PostgreSQL invalidates the slot, frees the WAL and keeps the leader up. The
/// replica then catches up from the S3 archive via `restore_command`, or
/// re-seeds through `create_replica_methods: [pgbackrest, basebackup]` — both
/// wired up by #78 — so on an archiving cluster the invalidation is usually
/// invisible. Without archiving it forces a re-clone off the leader, which is
/// still strictly better than losing the primary, so archiving changes only
/// *how tight* the cap is (via `SLOT_KEEP_FREE_PCT_*`), never *whether* there
/// is one.
///
/// The cap is the lesser of a total-based and a free-space-based bound:
///
/// - **free-space bound** is the one that keeps the primary alive. The disk
///   fills when `data + retained WAL` exceeds the volume, so the headroom that
///   matters is what's still empty, not the size of the disk.
/// - **total bound** covers the case free space can't: it's sampled once at
///   startup, and a node that has just re-cloned has a nearly-empty data dir, so
///   free space momentarily looks abundant while the data is about to grow into
///   it.
///
/// Taking the lesser means each covers the other's blind spot, and it removes
/// any need for an absolute floor to be large — on a 500 MB volume the bounds
/// simply come out small, instead of a fixed floor overshooting the disk.
///
/// Sized per-node off the local volume rather than seeded cluster-wide into
/// `bootstrap.dcs`: the value is a property of the disk under *this* node, any
/// member can become leader, and `bootstrap.dcs` only applies at cluster
/// genesis — an existing cluster would never pick it up.
///
/// An unmeasurable volume falls back to `-1`, preserving today's behaviour
/// rather than guessing a cap that could invalidate slots wrongly.
fn resolve_max_slot_wal_keep_size(
    env_value: Option<String>,
    volume_total_mib: u64,
    volume_free_mib: u64,
    archiving_enabled: bool,
) -> String {
    const UNLIMITED: &str = "-1";

    if let Some(v) = env_value.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        if is_valid_slot_keep_size(&v) {
            return v;
        }
        warn!(
            value = %v,
            "invalid POSTGRES_MAX_SLOT_WAL_KEEP_SIZE (need -1 or a positive PG size like 512GB); deriving from volume size"
        );
    }

    if volume_total_mib == 0 {
        warn!(
            "volume size unknown; leaving max_slot_wal_keep_size unlimited (a lagging replica's slot can fill the leader's disk)"
        );
        return UNLIMITED.to_string();
    }

    match derive_slot_keep_mib(volume_total_mib, volume_free_mib, archiving_enabled) {
        Some(mib) => {
            info!(
                volume_total_mib,
                volume_free_mib,
                archiving_enabled,
                max_slot_wal_keep_size_mib = mib,
                "sized max_slot_wal_keep_size from volume free space"
            );
            format!("{mib}MB")
        }
        None => {
            warn!(
                "volume size unknown; leaving max_slot_wal_keep_size unlimited (a lagging replica's slot can fill the leader's disk)"
            );
            UNLIMITED.to_string()
        }
    }
}

/// The numeric cap in MiB, or `None` when the volume can't be measured (caller
/// renders that as `-1`, unlimited). Split out from
/// [`resolve_max_slot_wal_keep_size`] so the leader-side reconciler can
/// re-derive the same value from *live* free space without re-parsing the env
/// override — `Config::from_env` samples free space once at startup, and a
/// long-lived leader whose data grows needs the cap recomputed against current
/// free space, not the boot-time reading.
pub(crate) fn derive_slot_keep_mib(
    volume_total_mib: u64,
    volume_free_mib: u64,
    archiving_enabled: bool,
) -> Option<u64> {
    if volume_total_mib == 0 {
        return None;
    }

    let free_pct = if archiving_enabled {
        SLOT_KEEP_FREE_PCT_ARCHIVING
    } else {
        SLOT_KEEP_FREE_PCT_NO_ARCHIVE
    };
    let by_total = volume_total_mib * SLOT_KEEP_TOTAL_PCT / 100;
    let by_free = volume_free_mib * free_pct / 100;

    // The floor keeps the cap usable on a nearly-full disk, but must never push
    // it back above the total bound — otherwise it reintroduces the very
    // unbounded behaviour this exists to stop. `.max(1)` because 0 is not
    // "small", it's a distinct PG mode that refuses to retain WAL for slots at
    // all.
    Some(
        by_total
            .min(by_free)
            .max(SLOT_KEEP_FLOOR_MIB)
            .min(by_total)
            .max(1),
    )
}

/// The operator's `POSTGRES_MAX_SLOT_WAL_KEEP_SIZE`, if set to a valid value.
/// `Some` means the operator pinned the cap by hand, and the leader-side
/// reconciler must leave it alone rather than override it with a derived value.
pub(crate) fn slot_keep_operator_override() -> Option<String> {
    env::var("POSTGRES_MAX_SLOT_WAL_KEEP_SIZE")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && is_valid_slot_keep_size(s))
}

/// Accept `-1` (explicitly unlimited) or a positive PG size string. PG's size
/// units are powers of 1024 and a bare integer means MB for this GUC, so bare
/// digits are safe here — unlike `POSTGRES_BASEBACKUP_MAX_RATE`, where a bare
/// value silently means kB/s.
fn is_valid_slot_keep_size(v: &str) -> bool {
    if v == "-1" {
        return true;
    }
    let digits = v
        .strip_suffix("kB")
        .or_else(|| v.strip_suffix("MB"))
        .or_else(|| v.strip_suffix("GB"))
        .or_else(|| v.strip_suffix("TB"))
        .unwrap_or(v)
        .trim();
    digits.parse::<u64>().is_ok_and(|n| n > 0)
}

/// `(total, available)` for the filesystem holding `path`, in MiB. `(0, 0)` when
/// it can't be measured. Mirrors `compute_volume_thresholds`' probe in
/// patroni_runner.
///
/// Uses `blocks_available` (what an unprivileged process can actually claim)
/// rather than `blocks_free`, which counts the root-reserved blocks Postgres
/// will never get to use.
pub(crate) fn volume_total_and_free_mib(path: &str) -> (u64, u64) {
    use nix::sys::statvfs::statvfs;

    statvfs(std::path::Path::new(path))
        .ok()
        .map(|s| {
            let frag = s.fragment_size() as u64;
            let mib = |blocks: u64| blocks.saturating_mul(frag) / (1024 * 1024);
            (mib(s.blocks() as u64), mib(s.blocks_available() as u64))
        })
        .unwrap_or((0, 0))
}

/// Validate an operator-supplied pg_basebackup --max-rate override. Requires
/// an explicit `k` or `M` suffix (matching pg_basebackup's case-sensitive
/// units) within pg_basebackup's accepted range of 32kB..1024MB. Bare digits
/// are rejected even though pg_basebackup accepts them: they mean kB/s, a
/// ~1000x unit footgun on a knob that is only ever hand-set mid-incident.
/// Anything invalid falls back to the default with a warning — the throttled
/// basebackup is the bootstrap path of last resort and must never be broken
/// by a typo.
fn resolve_basebackup_max_rate(env_value: Option<String>) -> String {
    const DEFAULT: &str = "20M";
    let Some(v) = env_value.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) else {
        return DEFAULT.to_string();
    };
    let kb = v
        .strip_suffix('k')
        .and_then(|d| d.parse::<u64>().ok())
        .or_else(|| {
            v.strip_suffix('M')
                .and_then(|d| d.parse::<u64>().ok())
                .and_then(|m| m.checked_mul(1024))
        });
    match kb {
        Some(kb) if (32..=1_048_576).contains(&kb) => v,
        _ => {
            warn!(
                value = %v,
                default = DEFAULT,
                "invalid POSTGRES_BASEBACKUP_MAX_RATE (need 32k..1024M, explicit k/M suffix); using default"
            );
            DEFAULT.to_string()
        }
    }
}

impl Config {
    /// Load configuration from environment variables
    pub fn from_env() -> Result<Self> {
        let name = String::env_required("PATRONI_NAME")?;
        let connect_address = String::env_required("RAILWAY_PRIVATE_DOMAIN")?;
        let etcd_hosts = String::env_required("PATRONI_ETCD3_HOSTS")?;

        let wal_archive_bucket = env::var("WAL_ARCHIVE_BUCKET").ok().filter(|s| !s.is_empty());
        let (volume_total_mib, volume_free_mib) = volume_total_and_free_mib(&crate::volume_root());
        let max_slot_wal_keep_size = resolve_max_slot_wal_keep_size(
            env::var("POSTGRES_MAX_SLOT_WAL_KEEP_SIZE").ok(),
            volume_total_mib,
            volume_free_mib,
            wal_archive_bucket.is_some(),
        );

        Ok(Self {
            scope: String::env_or("PATRONI_SCOPE", "railway-pg-ha"),
            name,
            connect_address,
            etcd_hosts,
            superuser: String::env_or("PATRONI_SUPERUSER_USERNAME", "postgres"),
            superuser_pass: env::var("PATRONI_SUPERUSER_PASSWORD").unwrap_or_default(),
            repl_user: String::env_or("PATRONI_REPLICATION_USERNAME", "replicator"),
            repl_pass: env::var("PATRONI_REPLICATION_PASSWORD").unwrap_or_default(),
            app_user: String::env_or("POSTGRES_USER", "postgres"),
            app_pass: env::var("POSTGRES_PASSWORD").unwrap_or_default(),
            app_db: env::var("POSTGRES_DB")
                .or_else(|_| env::var("PGDATABASE"))
                .unwrap_or_else(|_| "railway".to_string()),
            data_dir: pgdata(),
            certs_dir: ssl_dir(),
            // Constraint: loop_wait + 2*retry_timeout <= ttl
            // 10 + 2*17 = 44 <= 45 (1s buffer)
            ttl: String::env_or("PATRONI_TTL", "45"),
            loop_wait: String::env_or("PATRONI_LOOP_WAIT", "10"),
            retry_timeout: String::env_or("PATRONI_RETRY_TIMEOUT", "17"),
            health_check_interval: u64::env_parse("PATRONI_HEALTH_CHECK_INTERVAL", 5),
            health_check_timeout: u64::env_parse("PATRONI_HEALTH_CHECK_TIMEOUT", 5),
            max_failures: u32::env_parse("PATRONI_MAX_HEALTH_FAILURES", 3),
            startup_grace_period: u64::env_parse("PATRONI_STARTUP_GRACE_PERIOD", 60),
            max_startup_timeout: u64::env_parse("PATRONI_MAX_STARTUP_TIMEOUT", 1800),
            adopt_existing_data: bool::env_parse("PATRONI_ADOPT_EXISTING_DATA", false),
            wait_for_leader: bool::env_parse("PATRONI_WAIT_FOR_LEADER", false),
            synchronous_mode: bool::env_parse("PATRONI_SYNCHRONOUS_MODE", false),
            wal_archive_bucket,
            wal_recover_from_bucket: env::var("WAL_RECOVER_FROM_BUCKET")
                .ok()
                .filter(|s| !s.is_empty()),
            pitr_target_time: env::var("POSTGRES_RECOVERY_TARGET_TIME")
                .ok()
                .filter(|s| !s.is_empty()),
            pitr_target_xid: env::var("POSTGRES_RECOVERY_TARGET_XID")
                .ok()
                .filter(|s| !s.is_empty()),
            archive_timeout_secs: env::var("POSTGRES_ARCHIVE_TIMEOUT")
                .ok()
                .and_then(|s| s.parse::<i64>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(60),
            basebackup_max_rate: resolve_basebackup_max_rate(
                env::var("POSTGRES_BASEBACKUP_MAX_RATE").ok(),
            ),
            max_slot_wal_keep_size,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        resolve_basebackup_max_rate, resolve_max_slot_wal_keep_size, SLOT_KEEP_FLOOR_MIB,
        SLOT_KEEP_TOTAL_PCT,
    };

    /// The rampmetrics-api geometry that PANICked on 2026-07-10: a 2 TB volume
    /// holding ~1226 GB (data + WAL) at the time of measurement.
    const TWO_TB_MIB: u64 = 2_000_000_000_000 / (1024 * 1024);
    const TWO_TB_USED_MIB: u64 = 1_226_000_000_000 / (1024 * 1024);
    const TWO_TB_FREE_MIB: u64 = TWO_TB_MIB - TWO_TB_USED_MIB;

    /// The fleet's smallest volume: 500 Railway MB (= 1e6 bytes) ≈ 476 MiB.
    /// 14 postgres HA nodes run on these.
    const FIVE_HUNDRED_MB_MIB: u64 = 500 * 1_000_000 / (1024 * 1024);

    fn cap_mib(s: &str) -> u64 {
        s.trim_end_matches("MB").parse().expect("derived cap is MB")
    }

    #[test]
    fn slot_keep_size_is_bounded_by_free_space_not_just_total() {
        // Free space is the bound that actually protects the primary: 50% of
        // the 2 TB volume's remaining ~774 GB, which is tighter than 25% of
        // total, so it wins.
        let cap = cap_mib(&resolve_max_slot_wal_keep_size(
            None,
            TWO_TB_MIB,
            TWO_TB_FREE_MIB,
            true,
        ));
        assert_eq!(cap, TWO_TB_FREE_MIB / 2);
        assert!(cap < TWO_TB_MIB / 4, "free bound must beat the total bound here");
    }

    #[test]
    fn slot_keep_size_would_have_survived_the_2026_07_10_incident() {
        // The leader retained ~900 GB of WAL for a lagging replica's slot before
        // the 2 TB volume filled. The cap must trip well short of that and still
        // leave the ~1226 GB already on disk room to sit.
        let cap = cap_mib(&resolve_max_slot_wal_keep_size(
            None,
            TWO_TB_MIB,
            TWO_TB_FREE_MIB,
            true,
        ));
        let cap_gb = cap * 1024 * 1024 / 1_000_000_000;
        assert!(cap_gb < 900, "cap {cap_gb} GB must trip before the ~900 GB that filled the disk");
        assert!(
            1226 + cap_gb < 2000,
            "used + capped WAL ({} GB) must fit the 2 TB volume",
            1226 + cap_gb
        );
    }

    #[test]
    fn slot_keep_size_never_exceeds_a_500mb_volume() {
        // Regression: an earlier 1 GiB floor was larger than the entire 476 MiB
        // volume, so the slot could never be invalidated and the leader could
        // still fill up — the fix silently degraded to the unbounded default on
        // exactly the 14 smallest nodes in the fleet.
        for free in [FIVE_HUNDRED_MB_MIB, FIVE_HUNDRED_MB_MIB / 2, 10, 0] {
            for archiving in [true, false] {
                let cap = cap_mib(&resolve_max_slot_wal_keep_size(
                    None,
                    FIVE_HUNDRED_MB_MIB,
                    free,
                    archiving,
                ));
                assert!(
                    cap < FIVE_HUNDRED_MB_MIB,
                    "cap {cap} MiB must stay under the {FIVE_HUNDRED_MB_MIB} MiB volume (free={free}, archiving={archiving})"
                );
                assert!(cap > 0, "cap must never be 0 — that's a distinct PG mode");
            }
        }
    }

    #[test]
    fn slot_keep_size_total_bound_covers_the_freshly_cloned_node() {
        // A just-re-cloned node has an empty data dir, so free space looks like
        // the whole volume. The free bound alone would hand out 50% of a 2 TB
        // disk; the total bound holds it to 25% so the data can grow into the
        // rest.
        let cap = cap_mib(&resolve_max_slot_wal_keep_size(None, TWO_TB_MIB, TWO_TB_MIB, true));
        assert_eq!(cap, TWO_TB_MIB / 4);
    }

    #[test]
    fn slot_keep_size_is_tighter_when_the_archive_can_recover_the_replica() {
        // With S3 the replica catches up via restore_command or re-seeds from
        // the archive (#78), so invalidation is cheap and we bias toward primary
        // survival. Without it, invalidation forces a basebackup re-clone off the
        // live leader, so leave more slack before tripping.
        let with_s3 = cap_mib(&resolve_max_slot_wal_keep_size(
            None,
            TWO_TB_MIB,
            TWO_TB_FREE_MIB,
            true,
        ));
        let without_s3 = cap_mib(&resolve_max_slot_wal_keep_size(
            None,
            TWO_TB_MIB,
            TWO_TB_FREE_MIB,
            false,
        ));
        assert!(
            with_s3 < without_s3,
            "archiving should tighten the cap ({with_s3} vs {without_s3} MiB)"
        );
        // Both stay bounded — no-archive is never a licence to fill the disk.
        assert!(without_s3 <= TWO_TB_MIB / 4);
    }

    #[test]
    fn slot_keep_size_floor_stays_functional_but_never_beats_the_total_bound() {
        // Nearly-full disk: the free bound collapses toward zero. The floor keeps
        // the cap usable instead of invalidating slots on every hiccup, but must
        // not climb back above the total bound.
        let cap = cap_mib(&resolve_max_slot_wal_keep_size(None, TWO_TB_MIB, 0, true));
        assert_eq!(cap, SLOT_KEEP_FLOOR_MIB);

        // On a volume so small the floor would overshoot, the total bound wins.
        let tiny = cap_mib(&resolve_max_slot_wal_keep_size(None, 100, 0, true));
        assert!(tiny <= 100 * SLOT_KEEP_TOTAL_PCT / 100);
        assert!(tiny > 0);
    }

    #[test]
    fn slot_keep_size_unmeasurable_volume_stays_unlimited() {
        // Never guess a cap we can't size: an unmeasurable volume keeps PG's
        // default rather than risk invalidating slots wrongly.
        assert_eq!(resolve_max_slot_wal_keep_size(None, 0, 0, true), "-1");
    }

    #[test]
    fn slot_keep_size_accepts_operator_override() {
        assert_eq!(
            resolve_max_slot_wal_keep_size(Some("512GB".into()), TWO_TB_MIB, TWO_TB_FREE_MIB, true),
            "512GB"
        );
        // Explicitly opting back into unlimited is a valid escape hatch.
        assert_eq!(
            resolve_max_slot_wal_keep_size(Some("-1".into()), TWO_TB_MIB, TWO_TB_FREE_MIB, true),
            "-1"
        );
        // Bare digits mean MB for this GUC — no unit footgun, so accept them.
        assert_eq!(
            resolve_max_slot_wal_keep_size(Some("4096".into()), TWO_TB_MIB, TWO_TB_FREE_MIB, true),
            "4096"
        );
    }

    #[test]
    fn slot_keep_size_rejects_junk_override_and_derives_instead() {
        for junk in ["0", "abc", "-5", "12MiB", ""] {
            assert_eq!(
                resolve_max_slot_wal_keep_size(Some(junk.into()), TWO_TB_MIB, TWO_TB_FREE_MIB, true),
                format!("{}MB", TWO_TB_FREE_MIB / 2),
                "junk override {junk:?} must fall back to the derived cap"
            );
        }
    }

    #[test]
    fn max_rate_accepts_suffixed_values_in_range() {
        assert_eq!(resolve_basebackup_max_rate(Some("64M".into())), "64M");
        assert_eq!(resolve_basebackup_max_rate(Some("512k".into())), "512k");
        assert_eq!(resolve_basebackup_max_rate(Some("32k".into())), "32k");
        assert_eq!(resolve_basebackup_max_rate(Some("1024M".into())), "1024M");
        assert_eq!(resolve_basebackup_max_rate(Some(" 48M ".into())), "48M");
    }

    #[test]
    fn max_rate_falls_back_on_missing_or_malformed() {
        assert_eq!(resolve_basebackup_max_rate(None), "20M");
        assert_eq!(resolve_basebackup_max_rate(Some(String::new())), "20M");
        assert_eq!(resolve_basebackup_max_rate(Some("fast".into())), "20M");
        assert_eq!(resolve_basebackup_max_rate(Some("M".into())), "20M");
        // pg_basebackup's units are case-sensitive: only k and M.
        assert_eq!(resolve_basebackup_max_rate(Some("64m".into())), "20M");
        assert_eq!(resolve_basebackup_max_rate(Some("32MB".into())), "20M");
        assert_eq!(resolve_basebackup_max_rate(Some("-5M".into())), "20M");
    }

    #[test]
    fn max_rate_rejects_bare_digits_unit_footgun() {
        // 100 would mean 100 kB/s to pg_basebackup — ~200x slower than the
        // default, silently. Require the explicit suffix.
        assert_eq!(resolve_basebackup_max_rate(Some("100".into())), "20M");
    }

    #[test]
    fn max_rate_enforces_pg_basebackup_range() {
        assert_eq!(resolve_basebackup_max_rate(Some("31k".into())), "20M");
        assert_eq!(resolve_basebackup_max_rate(Some("1025M".into())), "20M");
        assert_eq!(resolve_basebackup_max_rate(Some("0M".into())), "20M");
        // Overflow-sized digit strings must not panic or pass.
        assert_eq!(
            resolve_basebackup_max_rate(Some("99999999999999999999M".into())),
            "20M"
        );
    }
}
