#!/usr/bin/env python3
"""Image regression: run as postgres, without a database or remote repository."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


def run(binary, args, env):
    return subprocess.run(
        [binary, *args], env=env, capture_output=True, text=True, timeout=15
    )


def check(condition, detail):
    if not condition:
        raise AssertionError(detail)


def main():
    native = "/usr/bin/pgbackrest"
    check(shutil.which("pgbackrest") == "/usr/local/bin/pgbackrest", "launcher missing from PATH")
    # Spaces in both paths exercise argument forwarding through the launcher.
    with tempfile.TemporaryDirectory(prefix="pgbackrest env test ") as tmp:
        repo = Path(tmp, "empty repo")
        repo.mkdir()
        conf = Path(tmp, "pgbackrest.conf")
        workers = {"backup": "1", "archive-push": "3", "archive-get": "3", "restore": "24"}
        conf.write_text(
            "[global]\nrepo1-type=posix\n"
            f"repo1-path={repo}\nlog-level-file=off\n"
            + "".join(f"[global:{cmd}]\nprocess-max={count}\n" for cmd, count in workers.items())
        )
        # Isolate the fixture from the caller's repository and credentials.
        env = {key: value for key, value in os.environ.items() if not key.startswith("PGBACKREST_")}
        overrides = {
            "PGBACKREST_BACKUP_PROCESS_MAX": "1",
            "PGBACKREST_ARCHIVE_PUSH_PROCESS_MAX": "3",
            "PGBACKREST_ARCHIVE_GET_PROCESS_MAX": "3",
            "PGBACKREST_RESTORE_PROCESS_MAX": "24",
            "PGBACKREST_DROP_THRESHOLD_MB": "5120",
        }
        args = [f"--config={conf}", "--output=json", "info"]
        # Exercise each override independently, then the combination.
        for settings in [{key: value} for key, value in overrides.items()] + [overrides]:
            child = dict(env, **settings)
            fixed = run("pgbackrest", args, child)
            check(fixed.returncode == 0, (fixed.returncode, fixed.stdout, fixed.stderr))
            check(isinstance(json.loads(fixed.stdout), list), fixed.stdout)
            check("environment contains invalid option" not in fixed.stdout + fixed.stderr, fixed)

        child = dict(env, **overrides)
        original = run(native, args, child)
        check(original.returncode == 0, (original.returncode, original.stderr))
        check("environment contains invalid option" in original.stdout, original.stdout)
        try:
            json.loads(original.stdout)
        except json.JSONDecodeError:
            pass
        else:
            raise AssertionError("unwrapped binary did not reproduce the JSON regression")
        print("PASS: native binary reproduces exit-zero invalid JSON; launcher returns valid JSON")

        for command, count in workers.items():
            help_result = run("pgbackrest", [f"--config={conf}", "help", command, "process-max"], child)
            check(help_result.returncode == 0, help_result.stderr)
            check(f"current: {count}" in help_result.stdout, help_result.stdout)
        print("PASS: command-specific process-max remains 1/3/3/24")

        # A native override still takes precedence over the config, and a
        # genuine configuration failure retains the native nonzero status.
        bad_env = dict(child, PGBACKREST_REPO1_TYPE="not-a-repo-type")
        original_bad = run(native, args, bad_env)
        fixed_bad = run("pgbackrest", args, bad_env)
        check(original_bad.returncode != 0, original_bad)
        check(fixed_bad.returncode == original_bad.returncode, fixed_bad)
        print("PASS: native environment precedence and failure status are preserved")

        unknown = run("pgbackrest", args, dict(env, PGBACKREST_UNKNOWN_ENV_TEST="1"))
        check("environment contains invalid option" in unknown.stdout, unknown.stdout)
        print("PASS: unrelated configuration warnings are still visible")


if __name__ == "__main__":
    main()
