#!/usr/bin/env bash
# test/unit-pgbackrest-wrappers.sh — fast, docker-free unit tests for
# postgres-patroni/pgbackrest-replica-restore-wrapper.sh and
# pgbackrest-archive-get-wrapper.sh (PR #78).
#
# These two scripts carry the trickiest logic in the PR — per-cluster
# repo1-path resolution at call time, with a DCS/marker/env priority order
# that differs by direction (DCS-first for the replica restore, marker-first
# for archive-get) — and are otherwise only exercised indirectly, deep inside
# a full Docker e2e run. This harness runs the REAL wrapper scripts (not a
# reimplementation) against fake `pgbackrest` and `curl` binaries prepended
# onto PATH, so every branch (DCS hit/miss/unreachable, marker present/
# empty/stale, env-default fallback, atomic marker rewrite, non-zero exit
# propagation) is a sub-second, deterministic test instead of a multi-minute
# docker scenario.
#
# Run: ./test/unit-pgbackrest-wrappers.sh
# Or:  ./test/unit-pgbackrest-wrappers.sh t_replica_restore_dcs_path_wins   # subset

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RESTORE_WRAPPER="${REPO_ROOT}/postgres-patroni/pgbackrest-replica-restore-wrapper.sh"
ARCHIVE_GET_WRAPPER="${REPO_ROOT}/postgres-patroni/pgbackrest-archive-get-wrapper.sh"

PASS=0
FAIL=0
FAILED_TESTS=()

if [ -t 1 ]; then
  R=$'\033[31m'; G=$'\033[32m'; Y=$'\033[33m'; B=$'\033[36m'; N=$'\033[0m'
else
  R=""; G=""; Y=""; B=""; N=""
fi
log()  { echo "${B}==>${N} $*"; }
ok()   { echo "${G}PASS${N} $*"; PASS=$((PASS+1)); }
ko()   { echo "${R}FAIL${N} $1: $2"; FAIL=$((FAIL+1)); FAILED_TESTS+=("$1"); }

# ----- fixture sandbox -------------------------------------------------------
# Each test gets its own TMPROOT with:
#   $TMPROOT/bin/       — fake pgbackrest + curl, prepended onto PATH
#   $TMPROOT/pgdata/    — fake PGDATA (marker lives at .pgbackrest_repo_path)
#   $TMPROOT/control/   — files the fakes read to decide behavior + logs they write
TMPROOT=""

setup() {
  TMPROOT="$(mktemp -d)"
  mkdir -p "$TMPROOT/bin" "$TMPROOT/pgdata" "$TMPROOT/control"

  # Fake pgbackrest: succeeds iff $PGBACKREST_REPO1_PATH equals the content of
  # control/success_path (if that file exists); otherwise exits with
  # control/fail_code (default 1). Always logs "call: <repo1-path or <unset>>"
  # so tests can assert call count + which path each call used.
  cat > "$TMPROOT/bin/pgbackrest" <<'EOF'
#!/usr/bin/env bash
CONTROL="$FAKE_CONTROL_DIR"
echo "call: ${PGBACKREST_REPO1_PATH:-<unset>}" >> "$CONTROL/pgbackrest_calls.log"
if [ -f "$CONTROL/success_path" ]; then
  want="$(cat "$CONTROL/success_path")"
  if [ "${PGBACKREST_REPO1_PATH:-}" = "$want" ]; then
    exit 0
  fi
fi
# miss_path: this path ANSWERS but lacks the file (genuine miss, exit 1),
# while every other path still fails with fail_code — lets a test give the
# stale-path attempt a connectivity-class code and the DCS retry a miss.
if [ -f "$CONTROL/miss_path" ]; then
  want="$(cat "$CONTROL/miss_path")"
  if [ "${PGBACKREST_REPO1_PATH:-}" = "$want" ]; then
    exit 1
  fi
fi
if [ -f "$CONTROL/fail_code" ]; then
  exit "$(cat "$CONTROL/fail_code")"
fi
exit 1
EOF
  chmod +x "$TMPROOT/bin/pgbackrest"

  # Fake curl: only ever called by the wrappers against localhost:8008/config.
  # Behavior driven by control/dcs_response (raw body to print) and
  # control/dcs_unreachable (if present, exit 7 with no output — curl's real
  # "couldn't connect" code). Logs every call's URL.
  cat > "$TMPROOT/bin/curl" <<'EOF'
#!/usr/bin/env bash
CONTROL="$FAKE_CONTROL_DIR"
# Last arg is the URL for every invocation the wrappers make.
url="${*: -1}"
echo "call: $url" >> "$CONTROL/curl_calls.log"
if [ -f "$CONTROL/dcs_unreachable" ]; then
  exit 7
fi
if [ -f "$CONTROL/dcs_response" ]; then
  cat "$CONTROL/dcs_response"
fi
exit 0
EOF
  chmod +x "$TMPROOT/bin/curl"
}

teardown() {
  [ -n "$TMPROOT" ] && rm -rf "$TMPROOT"
  TMPROOT=""
}

# Run a wrapper with the fixture PATH/env wired in. Captures stdout+stderr to
# $TMPROOT/control/output and the exit code to $TMPROOT/control/exit_code so
# assertions can inspect either after the fact (the wrapper is invoked in a
# subshell via `run`, so `$?` alone would be lost across helper calls).
run() {
  local wrapper="$1"; shift
  PATH="$TMPROOT/bin:$PATH" \
  FAKE_CONTROL_DIR="$TMPROOT/control" \
  PGDATA="$TMPROOT/pgdata" \
  bash "$wrapper" "$@" > "$TMPROOT/control/output" 2>&1
  echo "$?" > "$TMPROOT/control/exit_code"
}

exit_code() { cat "$TMPROOT/control/exit_code"; }
pgbackrest_call_count() { [ -f "$TMPROOT/control/pgbackrest_calls.log" ] && wc -l < "$TMPROOT/control/pgbackrest_calls.log" || echo 0; }
curl_call_count() { [ -f "$TMPROOT/control/curl_calls.log" ] && wc -l < "$TMPROOT/control/curl_calls.log" || echo 0; }
marker_path() { echo "$TMPROOT/pgdata/.pgbackrest_repo_path"; }
marker_content() { tr -d '\n\r' < "$(marker_path)" 2>/dev/null || echo "<absent>"; }

assert_eq() {
  local actual="$1" expected="$2" msg="$3"
  [ "$actual" = "$expected" ] && return 0
  echo "  expected: $expected"
  echo "  actual:   $actual"
  echo "  msg:      $msg"
  return 1
}

dcs_json_for() { printf '{"pgbackrest_repo1_path":"%s"}' "$1" > "$TMPROOT/control/dcs_response"; }

# =============================================================================
# pgbackrest-replica-restore-wrapper.sh (create_replica_method)
# =============================================================================

t_replica_restore_bucket_unset_noop() {
  setup
  run "$RESTORE_WRAPPER"   # WAL_ARCHIVE_BUCKET deliberately unset
  local rc; rc=$(exit_code)
  if [ "$rc" != "1" ]; then ko "$FUNCNAME" "expected exit 1, got $rc"; teardown; return; fi
  if [ "$(pgbackrest_call_count)" != "0" ]; then ko "$FUNCNAME" "pgbackrest must never be invoked without WAL_ARCHIVE_BUCKET"; teardown; return; fi
  ok "$FUNCNAME"
  teardown
}

t_replica_restore_dcs_path_wins_over_marker_and_env() {
  setup
  echo "s3-path-from-marker" > "$(marker_path)"
  dcs_json_for "s3-path-from-dcs"
  echo "s3-path-from-dcs" > "$TMPROOT/control/success_path"
  WAL_ARCHIVE_BUCKET=bucket PGBACKREST_REPO1_PATH=s3-path-from-env run "$RESTORE_WRAPPER"
  local rc; rc=$(exit_code)
  if [ "$rc" != "0" ]; then ko "$FUNCNAME" "expected exit 0, got $rc; output: $(cat "$TMPROOT/control/output")"; teardown; return; fi
  if ! grep -q "call: s3-path-from-dcs" "$TMPROOT/control/pgbackrest_calls.log"; then
    ko "$FUNCNAME" "pgbackrest was not called with the DCS-resolved path: $(cat "$TMPROOT/control/pgbackrest_calls.log")"; teardown; return
  fi
  assert_eq "$(marker_content)" "s3-path-from-dcs" "marker must be rewritten to the path actually used" || { ko "$FUNCNAME" "marker mismatch"; teardown; return; }
  ok "$FUNCNAME"
  teardown
}

t_replica_restore_falls_back_to_marker_when_dcs_unreachable() {
  setup
  echo "s3-path-from-marker" > "$(marker_path)"
  touch "$TMPROOT/control/dcs_unreachable"
  echo "s3-path-from-marker" > "$TMPROOT/control/success_path"
  WAL_ARCHIVE_BUCKET=bucket run "$RESTORE_WRAPPER"
  local rc; rc=$(exit_code)
  if [ "$rc" != "0" ]; then ko "$FUNCNAME" "expected exit 0, got $rc; output: $(cat "$TMPROOT/control/output")"; teardown; return; fi
  if ! grep -q "call: s3-path-from-marker" "$TMPROOT/control/pgbackrest_calls.log"; then
    ko "$FUNCNAME" "pgbackrest was not called with the marker path"; teardown; return
  fi
  ok "$FUNCNAME"
  teardown
}

t_replica_restore_falls_back_to_env_default_and_writes_marker_on_success() {
  setup
  # No marker file, DCS returns nothing usable (empty body — malformed JSON
  # from python3's perspective), env default is what pgbackrest is told to
  # accept — proving success writes the marker even on the env-default path.
  : > "$TMPROOT/control/dcs_response"
  echo "bucket-root" > "$TMPROOT/control/success_path"
  WAL_ARCHIVE_BUCKET=bucket PGBACKREST_REPO1_PATH=bucket-root run "$RESTORE_WRAPPER"
  local rc; rc=$(exit_code)
  if [ "$rc" != "0" ]; then ko "$FUNCNAME" "expected exit 0, got $rc; output: $(cat "$TMPROOT/control/output")"; teardown; return; fi
  assert_eq "$(marker_content)" "bucket-root" "env-default success must still write the marker" || { ko "$FUNCNAME" "marker mismatch"; teardown; return; }
  ok "$FUNCNAME"
  teardown
}

t_replica_restore_null_dcs_value_falls_through_to_marker() {
  setup
  echo "s3-path-from-marker" > "$(marker_path)"
  printf '{"pgbackrest_repo1_path":null}' > "$TMPROOT/control/dcs_response"
  echo "s3-path-from-marker" > "$TMPROOT/control/success_path"
  WAL_ARCHIVE_BUCKET=bucket run "$RESTORE_WRAPPER"
  local rc; rc=$(exit_code)
  if [ "$rc" != "0" ]; then ko "$FUNCNAME" "a JSON null pgbackrest_repo1_path must fall through to the marker, got rc=$rc: $(cat "$TMPROOT/control/output")"; teardown; return; fi
  ok "$FUNCNAME"
  teardown
}

t_replica_restore_failure_never_writes_marker() {
  setup
  echo "old-marker-value" > "$(marker_path)"
  dcs_json_for "some-path"
  echo "2" > "$TMPROOT/control/fail_code"   # no success_path -> pgbackrest always fails
  WAL_ARCHIVE_BUCKET=bucket run "$RESTORE_WRAPPER"
  local rc; rc=$(exit_code)
  assert_eq "$rc" "2" "the wrapper must propagate pgbackrest's real exit code" || { ko "$FUNCNAME" "exit code mismatch"; teardown; return; }
  assert_eq "$(marker_content)" "old-marker-value" "a failed restore must never overwrite an existing marker" || { ko "$FUNCNAME" "marker was overwritten on failure"; teardown; return; }
  ok "$FUNCNAME"
  teardown
}

t_replica_restore_creates_missing_pgdata_dir() {
  setup
  rm -rf "$TMPROOT/pgdata"   # wiped-volume scenario: not even the dir exists
  dcs_json_for "some-path"
  echo "some-path" > "$TMPROOT/control/success_path"
  WAL_ARCHIVE_BUCKET=bucket run "$RESTORE_WRAPPER"
  local rc; rc=$(exit_code)
  if [ "$rc" != "0" ]; then ko "$FUNCNAME" "expected exit 0, got $rc: $(cat "$TMPROOT/control/output")"; teardown; return; fi
  if [ ! -d "$TMPROOT/pgdata" ]; then ko "$FUNCNAME" "PGDATA must be created before restore runs"; teardown; return; fi
  ok "$FUNCNAME"
  teardown
}

t_replica_restore_marker_written_atomically_no_tmp_leftover() {
  setup
  dcs_json_for "some-path"
  echo "some-path" > "$TMPROOT/control/success_path"
  WAL_ARCHIVE_BUCKET=bucket run "$RESTORE_WRAPPER"
  local rc; rc=$(exit_code)
  if [ "$rc" != "0" ]; then ko "$FUNCNAME" "expected exit 0, got $rc"; teardown; return; fi
  if find "$TMPROOT/pgdata" -maxdepth 1 -name '.pgbackrest_repo_path.tmp.*' | grep -q .; then
    ko "$FUNCNAME" "tmp marker file leaked (tmp+rename must clean up)"; teardown; return
  fi
  # GNU and BSD `stat` disagree on what `-f` even means (format flag vs.
  # filesystem-info mode), so neither flavor's "try one, fall back to the
  # other" is reliable across dev/CI hosts. python3 (already a hard
  # dependency of the wrappers themselves) gives a portable answer.
  local mode; mode=$(python3 -c "import os,sys; print(oct(os.stat(sys.argv[1]).st_mode & 0o777)[2:])" "$(marker_path)")
  assert_eq "$mode" "640" "marker must be 640" || { ko "$FUNCNAME" "marker perms mismatch"; teardown; return; }
  ok "$FUNCNAME"
  teardown
}

# =============================================================================
# pgbackrest-archive-get-wrapper.sh (restore_command)
# =============================================================================

t_archive_get_bucket_unset_noop() {
  setup
  run "$ARCHIVE_GET_WRAPPER" "00000001.history" "/tmp/dest"
  local rc; rc=$(exit_code)
  if [ "$rc" != "1" ]; then ko "$FUNCNAME" "expected exit 1, got $rc"; teardown; return; fi
  if [ "$(pgbackrest_call_count)" != "0" ]; then ko "$FUNCNAME" "pgbackrest must never be invoked without WAL_ARCHIVE_BUCKET"; teardown; return; fi
  ok "$FUNCNAME"
  teardown
}

t_archive_get_usage_error_on_missing_args() {
  setup
  WAL_ARCHIVE_BUCKET=bucket run "$ARCHIVE_GET_WRAPPER"
  local rc; rc=$(exit_code)
  assert_eq "$rc" "1" "missing wal_file/dest_path must exit 1" || { ko "$FUNCNAME" "exit code mismatch"; teardown; return; }
  if [ "$(pgbackrest_call_count)" != "0" ]; then ko "$FUNCNAME" "pgbackrest must never be invoked on a usage error"; teardown; return; fi
  ok "$FUNCNAME"
  teardown
}

t_archive_get_marker_hit_never_queries_dcs() {
  setup
  echo "correct-path" > "$(marker_path)"
  echo "correct-path" > "$TMPROOT/control/success_path"
  WAL_ARCHIVE_BUCKET=bucket run "$ARCHIVE_GET_WRAPPER" "000000010000000000000001" "/tmp/dest"
  local rc; rc=$(exit_code)
  if [ "$rc" != "0" ]; then ko "$FUNCNAME" "expected exit 0, got $rc: $(cat "$TMPROOT/control/output")"; teardown; return; fi
  assert_eq "$(curl_call_count)" "0" "a marker hit must never cost a DCS round-trip" || { ko "$FUNCNAME" "unexpected DCS query"; teardown; return; }
  assert_eq "$(pgbackrest_call_count)" "1" "marker hit must resolve in exactly one pgbackrest call" || { ko "$FUNCNAME" "call count mismatch"; teardown; return; }
  ok "$FUNCNAME"
  teardown
}

t_archive_get_empty_marker_falls_back_to_env_default() {
  setup
  : > "$(marker_path)"   # present but empty — truncated write / fresh file
  echo "env-default-path" > "$TMPROOT/control/success_path"
  WAL_ARCHIVE_BUCKET=bucket PGBACKREST_REPO1_PATH=env-default-path run "$ARCHIVE_GET_WRAPPER" "wal" "/tmp/dest"
  local rc; rc=$(exit_code)
  if [ "$rc" != "0" ]; then ko "$FUNCNAME" "expected exit 0, got $rc: $(cat "$TMPROOT/control/output")"; teardown; return; fi
  if ! grep -q "call: env-default-path" "$TMPROOT/control/pgbackrest_calls.log"; then
    ko "$FUNCNAME" "an empty marker must fall back to the inherited env default, not run with an empty path"; teardown; return
  fi
  ok "$FUNCNAME"
  teardown
}

t_archive_get_stale_marker_falls_back_to_dcs_and_rewrites() {
  setup
  echo "stale-marker-path" > "$(marker_path)"
  dcs_json_for "current-dcs-path"
  echo "current-dcs-path" > "$TMPROOT/control/success_path"
  WAL_ARCHIVE_BUCKET=bucket run "$ARCHIVE_GET_WRAPPER" "wal" "/tmp/dest"
  local rc; rc=$(exit_code)
  if [ "$rc" != "0" ]; then ko "$FUNCNAME" "expected exit 0, got $rc: $(cat "$TMPROOT/control/output")"; teardown; return; fi
  assert_eq "$(pgbackrest_call_count)" "2" "must try the marker path, miss, then retry at the DCS path" || { ko "$FUNCNAME" "call count mismatch: $(cat "$TMPROOT/control/pgbackrest_calls.log")"; teardown; return; }
  assert_eq "$(curl_call_count)" "1" "must consult DCS exactly once after the marker miss" || { ko "$FUNCNAME" "curl call count mismatch"; teardown; return; }
  assert_eq "$(marker_content)" "current-dcs-path" "marker must be rewritten to the DCS-resolved path that actually worked" || { ko "$FUNCNAME" "marker not rewritten"; teardown; return; }
  ok "$FUNCNAME"
  teardown
}

t_archive_get_genuine_miss_dcs_agrees_no_second_attempt() {
  setup
  echo "current-path" > "$(marker_path)"
  dcs_json_for "current-path"   # DCS agrees with the marker: a real miss, not staleness
  echo "1" > "$TMPROOT/control/fail_code"   # pgbackrest miss semantics: exit 1
  WAL_ARCHIVE_BUCKET=bucket run "$ARCHIVE_GET_WRAPPER" "wal" "/tmp/dest"
  local rc; rc=$(exit_code)
  assert_eq "$rc" "1" "a genuine miss (segment not archived yet) must return pgbackrest's miss code" || { ko "$FUNCNAME" "exit code mismatch"; teardown; return; }
  assert_eq "$(pgbackrest_call_count)" "1" "when DCS agrees with the marker there is nothing to retry — must not call pgbackrest a second time" || { ko "$FUNCNAME" "unexpected retry: $(cat "$TMPROOT/control/pgbackrest_calls.log")"; teardown; return; }
  assert_eq "$(marker_content)" "current-path" "marker must be untouched on a genuine miss" || { ko "$FUNCNAME" "marker changed unexpectedly"; teardown; return; }
  ok "$FUNCNAME"
  teardown
}

breaker_path() { echo "$TMPROOT/pgdata/.pgbackrest_archive_get_conn_failures"; }
# First field only: the breaker file is "<count> <epoch-of-latest-failure>".
breaker_count() { awk '{print $1}' < "$(breaker_path)" 2>/dev/null || echo "<absent>"; }

t_archive_get_connectivity_breaker_trips_at_threshold() {
  # Consecutive connectivity-class failures (rc>1: HostConnect/RepoInvalid
  # etc.) must trip to exit 126 — FATAL to Postgres — at the threshold, so
  # a standby wedged on a dead archive crash-loops (where the WAL-too-old
  # reinitialize can land) instead of retrying the endpoint forever.
  setup
  touch "$TMPROOT/control/dcs_unreachable"
  echo "103" > "$TMPROOT/control/fail_code"
  WAL_ARCHIVE_BUCKET=bucket WAL_ARCHIVE_GET_CONNECTIVITY_TRIP=3 run "$ARCHIVE_GET_WRAPPER" "wal" "/tmp/dest"
  assert_eq "$(exit_code)" "103" "failure 1/3 must pass through pgbackrest's code" || { ko "$FUNCNAME" "run1 rc"; teardown; return; }
  assert_eq "$(breaker_count)" "1" "failure 1/3 must persist a count of 1" || { ko "$FUNCNAME" "run1 counter"; teardown; return; }
  WAL_ARCHIVE_BUCKET=bucket WAL_ARCHIVE_GET_CONNECTIVITY_TRIP=3 run "$ARCHIVE_GET_WRAPPER" "wal" "/tmp/dest"
  assert_eq "$(exit_code)" "103" "failure 2/3 must pass through pgbackrest's code" || { ko "$FUNCNAME" "run2 rc"; teardown; return; }
  WAL_ARCHIVE_BUCKET=bucket WAL_ARCHIVE_GET_CONNECTIVITY_TRIP=3 run "$ARCHIVE_GET_WRAPPER" "wal" "/tmp/dest"
  assert_eq "$(exit_code)" "126" "failure 3/3 must trip the breaker with exit 126 (>125 = FATAL)" || { ko "$FUNCNAME" "run3 rc"; teardown; return; }
  if [ -f "$(breaker_path)" ]; then ko "$FUNCNAME" "trip must reset the counter file"; teardown; return; fi
  grep -q "connectivity breaker tripped" "$TMPROOT/control/output" || { ko "$FUNCNAME" "trip must log its reason"; teardown; return; }
  ok "$FUNCNAME"
  teardown
}

t_archive_get_genuine_miss_resets_breaker() {
  # rc=1 means the repo ANSWERED and the file is absent — proof of
  # connectivity, so it must clear the count. Only an unbroken run of
  # connectivity failures may trip; otherwise the normal end-of-catch-up
  # miss would inherit stale counts from an earlier blip.
  setup
  touch "$TMPROOT/control/dcs_unreachable"
  echo "103" > "$TMPROOT/control/fail_code"
  WAL_ARCHIVE_BUCKET=bucket WAL_ARCHIVE_GET_CONNECTIVITY_TRIP=3 run "$ARCHIVE_GET_WRAPPER" "wal" "/tmp/dest"
  WAL_ARCHIVE_BUCKET=bucket WAL_ARCHIVE_GET_CONNECTIVITY_TRIP=3 run "$ARCHIVE_GET_WRAPPER" "wal" "/tmp/dest"
  assert_eq "$(breaker_count)" "2" "two connectivity failures must persist a count of 2" || { ko "$FUNCNAME" "pre-miss counter"; teardown; return; }
  echo "1" > "$TMPROOT/control/fail_code"
  WAL_ARCHIVE_BUCKET=bucket WAL_ARCHIVE_GET_CONNECTIVITY_TRIP=3 run "$ARCHIVE_GET_WRAPPER" "wal" "/tmp/dest"
  assert_eq "$(exit_code)" "1" "a genuine miss must still exit 1" || { ko "$FUNCNAME" "miss rc"; teardown; return; }
  if [ -f "$(breaker_path)" ]; then ko "$FUNCNAME" "a genuine miss must clear the breaker"; teardown; return; fi
  echo "103" > "$TMPROOT/control/fail_code"
  WAL_ARCHIVE_BUCKET=bucket WAL_ARCHIVE_GET_CONNECTIVITY_TRIP=3 run "$ARCHIVE_GET_WRAPPER" "wal" "/tmp/dest"
  assert_eq "$(exit_code)" "103" "post-reset failure must count from 1 again, not trip" || { ko "$FUNCNAME" "post-reset rc"; teardown; return; }
  ok "$FUNCNAME"
  teardown
}

t_archive_get_success_resets_breaker() {
  setup
  touch "$TMPROOT/control/dcs_unreachable"
  echo "103" > "$TMPROOT/control/fail_code"
  WAL_ARCHIVE_BUCKET=bucket WAL_ARCHIVE_GET_CONNECTIVITY_TRIP=3 run "$ARCHIVE_GET_WRAPPER" "wal" "/tmp/dest"
  WAL_ARCHIVE_BUCKET=bucket WAL_ARCHIVE_GET_CONNECTIVITY_TRIP=3 run "$ARCHIVE_GET_WRAPPER" "wal" "/tmp/dest"
  echo "env-path" > "$TMPROOT/control/success_path"
  WAL_ARCHIVE_BUCKET=bucket WAL_ARCHIVE_GET_CONNECTIVITY_TRIP=3 PGBACKREST_REPO1_PATH=env-path run "$ARCHIVE_GET_WRAPPER" "wal" "/tmp/dest"
  assert_eq "$(exit_code)" "0" "recovered endpoint must serve the segment" || { ko "$FUNCNAME" "success rc"; teardown; return; }
  if [ -f "$(breaker_path)" ]; then ko "$FUNCNAME" "success must clear the breaker"; teardown; return; fi
  ok "$FUNCNAME"
  teardown
}

t_archive_get_post_trip_run_counts_fresh() {
  # Each crash-loop cycle must re-probe the endpoint a full threshold's
  # worth: the run AFTER a trip passes the code through and counts from 1.
  setup
  touch "$TMPROOT/control/dcs_unreachable"
  echo "103" > "$TMPROOT/control/fail_code"
  for _ in 1 2 3; do
    WAL_ARCHIVE_BUCKET=bucket WAL_ARCHIVE_GET_CONNECTIVITY_TRIP=3 run "$ARCHIVE_GET_WRAPPER" "wal" "/tmp/dest"
  done
  assert_eq "$(exit_code)" "126" "third failure must have tripped" || { ko "$FUNCNAME" "trip rc"; teardown; return; }
  WAL_ARCHIVE_BUCKET=bucket WAL_ARCHIVE_GET_CONNECTIVITY_TRIP=3 run "$ARCHIVE_GET_WRAPPER" "wal" "/tmp/dest"
  assert_eq "$(exit_code)" "103" "first post-trip failure must not trip again" || { ko "$FUNCNAME" "post-trip rc"; teardown; return; }
  assert_eq "$(breaker_count)" "1" "post-trip counting must restart at 1" || { ko "$FUNCNAME" "post-trip counter"; teardown; return; }
  ok "$FUNCNAME"
  teardown
}

t_archive_get_stale_breaker_count_restarts() {
  # Decay: a persisted count whose latest failure is older than the stale
  # window is a relic of an unrelated episode (e.g. streaming blips days
  # apart on a cluster whose archive answers but permanently errors — a
  # deleted bucket never produces a resetting 0/1). The next failure must
  # restart the count at 1, not push the accumulated total over the trip
  # threshold and FATAL an otherwise-healthy standby.
  setup
  touch "$TMPROOT/control/dcs_unreachable"
  echo "103" > "$TMPROOT/control/fail_code"
  printf '%s %s\n' 2 "$(( $(date +%s) - 7200 ))" > "$(breaker_path)"
  WAL_ARCHIVE_BUCKET=bucket WAL_ARCHIVE_GET_CONNECTIVITY_TRIP=3 run "$ARCHIVE_GET_WRAPPER" "wal" "/tmp/dest"
  assert_eq "$(exit_code)" "103" "a stale count must not trip (this would have been failure 3/3)" || { ko "$FUNCNAME" "rc"; teardown; return; }
  assert_eq "$(breaker_count)" "1" "a stale count must restart at 1" || { ko "$FUNCNAME" "counter"; teardown; return; }
  ok "$FUNCNAME"
  teardown
}

t_archive_get_fresh_breaker_count_accumulates_to_trip() {
  # Boundary companion to the stale test: a count whose latest failure is
  # RECENT is one continuous episode and must keep accumulating — seeded at
  # 2/3 with a fresh timestamp, the next failure trips.
  setup
  touch "$TMPROOT/control/dcs_unreachable"
  echo "103" > "$TMPROOT/control/fail_code"
  printf '%s %s\n' 2 "$(date +%s)" > "$(breaker_path)"
  WAL_ARCHIVE_BUCKET=bucket WAL_ARCHIVE_GET_CONNECTIVITY_TRIP=3 run "$ARCHIVE_GET_WRAPPER" "wal" "/tmp/dest"
  assert_eq "$(exit_code)" "126" "a fresh count must accumulate and trip at the threshold" || { ko "$FUNCNAME" "rc"; teardown; return; }
  ok "$FUNCNAME"
  teardown
}

t_archive_get_legacy_count_only_breaker_restarts() {
  # Rolling-upgrade safety: a pre-decay breaker file holds a bare count with
  # no timestamp — freshness is unprovable, so it must restart at 1 (the
  # slower-to-trip direction), not be trusted as current.
  setup
  touch "$TMPROOT/control/dcs_unreachable"
  echo "103" > "$TMPROOT/control/fail_code"
  printf '%s\n' 2 > "$(breaker_path)"
  WAL_ARCHIVE_BUCKET=bucket WAL_ARCHIVE_GET_CONNECTIVITY_TRIP=3 run "$ARCHIVE_GET_WRAPPER" "wal" "/tmp/dest"
  assert_eq "$(exit_code)" "103" "a legacy count-only file must not contribute to a trip" || { ko "$FUNCNAME" "rc"; teardown; return; }
  assert_eq "$(breaker_count)" "1" "a legacy count-only file must restart at 1" || { ko "$FUNCNAME" "counter"; teardown; return; }
  ok "$FUNCNAME"
  teardown
}

t_archive_get_dcs_unreachable_on_miss_returns_original_failure() {
  setup
  echo "marker-path" > "$(marker_path)"
  touch "$TMPROOT/control/dcs_unreachable"
  echo "5" > "$TMPROOT/control/fail_code"
  WAL_ARCHIVE_BUCKET=bucket run "$ARCHIVE_GET_WRAPPER" "wal" "/tmp/dest"
  local rc; rc=$(exit_code)
  assert_eq "$rc" "5" "DCS unreachable on a miss must still surface the original pgbackrest exit code" || { ko "$FUNCNAME" "exit code mismatch"; teardown; return; }
  assert_eq "$(pgbackrest_call_count)" "1" "must not retry when DCS never answered" || { ko "$FUNCNAME" "unexpected retry"; teardown; return; }
  ok "$FUNCNAME"
  teardown
}

t_archive_get_double_miss_returns_retry_exit_code() {
  # When the DCS-path retry also fails, the surfaced code is the RETRY's
  # own $retry_rc — the verdict against the authoritative path — not the
  # stale-path first attempt's. Here both fail with the same code, so this
  # pins the shape (exit code passthrough, both paths attempted, marker
  # untouched); the distinct-code case is covered by
  # t_archive_get_dcs_retry_miss_clears_breaker below.
  setup
  echo "marker-path" > "$(marker_path)"
  dcs_json_for "dcs-path"
  echo "9" > "$TMPROOT/control/fail_code"   # no success_path -> every attempt fails with 9
  WAL_ARCHIVE_BUCKET=bucket run "$ARCHIVE_GET_WRAPPER" "wal" "/tmp/dest"
  local rc; rc=$(exit_code)
  assert_eq "$rc" "9" "double-miss must surface the failing exit code" || { ko "$FUNCNAME" "exit code mismatch"; teardown; return; }
  assert_eq "$(pgbackrest_call_count)" "2" "must attempt both marker and DCS paths before giving up" || { ko "$FUNCNAME" "call count mismatch"; teardown; return; }
  assert_eq "$(marker_content)" "marker-path" "a failed retry must never rewrite the marker" || { ko "$FUNCNAME" "marker changed on failed retry"; teardown; return; }
  ok "$FUNCNAME"
  teardown
}

t_archive_get_dcs_retry_miss_clears_breaker() {
  # The scenario the retry-verdict capture exists for: a standby with a
  # stale marker gets a connectivity-class code (103) against the wrong
  # path, but the DCS-path retry ANSWERS with a genuine miss (1 — segment
  # simply not archived yet, normal at the leading edge of catch-up). The
  # wrapper must surface the retry's 1 and CLEAR the breaker: the
  # authoritative repo just proved reachable, so inheriting the stale
  # path's 103 would count a healthy catching-up standby toward the
  # connectivity trip on every not-yet-archived segment.
  setup
  echo "marker-path" > "$(marker_path)"
  dcs_json_for "dcs-path"
  echo "103" > "$TMPROOT/control/fail_code"      # stale-path attempt: connectivity-class
  echo "dcs-path" > "$TMPROOT/control/miss_path" # DCS retry: repo answers, file absent
  # Pre-seed a breaker count to prove the miss CLEARS it, not just fails to bump it.
  printf '2 %s\n' "$(date +%s)" > "$(breaker_path)"
  WAL_ARCHIVE_BUCKET=bucket run "$ARCHIVE_GET_WRAPPER" "wal" "/tmp/dest"
  assert_eq "$(exit_code)" "1" "the DCS retry's genuine miss must win over the stale path's 103" || { ko "$FUNCNAME" "exit code mismatch"; teardown; return; }
  assert_eq "$(pgbackrest_call_count)" "2" "must attempt both marker and DCS paths" || { ko "$FUNCNAME" "call count mismatch"; teardown; return; }
  if [ -f "$(breaker_path)" ]; then ko "$FUNCNAME" "a DCS-path miss proves connectivity and must clear the breaker"; teardown; return; fi
  assert_eq "$(marker_content)" "marker-path" "a miss can't vouch for the path; marker must not be rewritten" || { ko "$FUNCNAME" "marker changed on miss"; teardown; return; }
  ok "$FUNCNAME"
  teardown
}

# =============================================================================
ALL_TESTS=(
  t_replica_restore_bucket_unset_noop
  t_replica_restore_dcs_path_wins_over_marker_and_env
  t_replica_restore_falls_back_to_marker_when_dcs_unreachable
  t_replica_restore_falls_back_to_env_default_and_writes_marker_on_success
  t_replica_restore_null_dcs_value_falls_through_to_marker
  t_replica_restore_failure_never_writes_marker
  t_replica_restore_creates_missing_pgdata_dir
  t_replica_restore_marker_written_atomically_no_tmp_leftover
  t_archive_get_bucket_unset_noop
  t_archive_get_usage_error_on_missing_args
  t_archive_get_marker_hit_never_queries_dcs
  t_archive_get_empty_marker_falls_back_to_env_default
  t_archive_get_stale_marker_falls_back_to_dcs_and_rewrites
  t_archive_get_genuine_miss_dcs_agrees_no_second_attempt
  t_archive_get_dcs_unreachable_on_miss_returns_original_failure
  t_archive_get_double_miss_returns_retry_exit_code
  t_archive_get_dcs_retry_miss_clears_breaker
  t_archive_get_connectivity_breaker_trips_at_threshold
  t_archive_get_genuine_miss_resets_breaker
  t_archive_get_success_resets_breaker
  t_archive_get_post_trip_run_counts_fresh
  t_archive_get_stale_breaker_count_restarts
  t_archive_get_fresh_breaker_count_accumulates_to_trip
  t_archive_get_legacy_count_only_breaker_restarts
)

if [ "$#" -gt 0 ]; then
  TESTS_TO_RUN=("$@")
else
  TESTS_TO_RUN=("${ALL_TESTS[@]}")
fi

log "running ${#TESTS_TO_RUN[@]} wrapper unit test(s)"
for t in "${TESTS_TO_RUN[@]}"; do
  "$t"
done

echo
echo "${G}${PASS} passed${N}, ${R}${FAIL} failed${N}"
if [ "$FAIL" -gt 0 ]; then
  echo "Failed: ${FAILED_TESTS[*]}"
fi
exit "$FAIL"
