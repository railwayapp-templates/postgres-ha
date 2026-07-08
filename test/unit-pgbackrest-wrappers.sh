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
  echo "3" > "$TMPROOT/control/fail_code"   # no success_path -> always fails
  WAL_ARCHIVE_BUCKET=bucket run "$ARCHIVE_GET_WRAPPER" "wal" "/tmp/dest"
  local rc; rc=$(exit_code)
  assert_eq "$rc" "3" "a genuine miss (segment not archived yet) must return pgbackrest's real exit code" || { ko "$FUNCNAME" "exit code mismatch"; teardown; return; }
  assert_eq "$(pgbackrest_call_count)" "1" "when DCS agrees with the marker there is nothing to retry — must not call pgbackrest a second time" || { ko "$FUNCNAME" "unexpected retry: $(cat "$TMPROOT/control/pgbackrest_calls.log")"; teardown; return; }
  assert_eq "$(marker_content)" "current-path" "marker must be untouched on a genuine miss" || { ko "$FUNCNAME" "marker changed unexpectedly"; teardown; return; }
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

t_archive_get_double_miss_returns_first_attempts_exit_code() {
  # Sharp regression guard on a real subtlety: when the DCS-path retry ALSO
  # fails, the script's final `exit "$rc"` still refers to the FIRST
  # attempt's $rc (the retry's own status is only checked by the `if`, never
  # captured) — so a double-miss always surfaces the original code, not the
  # retry's, even when they differ. Exercise that with distinct fail codes to
  # prove which one actually wins.
  setup
  echo "marker-path" > "$(marker_path)"
  dcs_json_for "dcs-path"
  echo "9" > "$TMPROOT/control/fail_code"   # no success_path -> every attempt fails with 9
  WAL_ARCHIVE_BUCKET=bucket run "$ARCHIVE_GET_WRAPPER" "wal" "/tmp/dest"
  local rc; rc=$(exit_code)
  assert_eq "$rc" "9" "double-miss must surface the first attempt's exit code" || { ko "$FUNCNAME" "exit code mismatch"; teardown; return; }
  assert_eq "$(pgbackrest_call_count)" "2" "must attempt both marker and DCS paths before giving up" || { ko "$FUNCNAME" "call count mismatch"; teardown; return; }
  assert_eq "$(marker_content)" "marker-path" "a failed retry must never rewrite the marker" || { ko "$FUNCNAME" "marker changed on failed retry"; teardown; return; }
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
  t_archive_get_double_miss_returns_first_attempts_exit_code
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
