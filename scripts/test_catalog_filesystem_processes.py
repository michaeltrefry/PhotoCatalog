"""Run the explicitly ignored real-process fixtures with the built CLI."""

import argparse
import hashlib
import os
from pathlib import Path
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--executable", required=True, type=Path)
    parser.add_argument("--release", action="store_true")
    args = parser.parse_args()
    repository = Path(__file__).resolve().parent.parent
    executable = args.executable
    if not executable.is_absolute():
        executable = repository / executable
    executable = executable.resolve(strict=True)
    if not executable.is_file():
        parser.error("the configured CLI must be a regular file")
    digest = hashlib.sha256()
    with executable.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    print(f"Configured CLI: {executable}; SHA256 {digest.hexdigest()}", flush=True)
    environment = dict(os.environ, PHOTOCATALOG_TEST_EXECUTABLE=str(executable))
    tests = [
        "filesystem::tests::actual_queued_cancel_and_duplicate_ids_do_not_dispatch_f_effects",
        "filesystem::tests::actual_idle_f_death_is_monitored_but_normal_retirement_is_not_failure",
        "filesystem_tests::actual_f_and_eight_sql_roles_preserve_wal_write_lock_through_marker_and_close",
        "filesystem_tests::actual_confirm_loss_reaps_c74_before_f_retirement",
        "filesystem_tests::actual_lost_prepare_inspects_original_record_and_abandons_without_replay",
        "filesystem_tests::actual_f_protocol_restores_offline_import_root_for_export",
        "filesystem::preview_tests::actual_empty_config_actor_import_close_reopen_restores_export_authority",
    ]
    if os.name == "posix":
        tests.append("filesystem_tests::actual_f_binary_shm_alias_read_does_not_release_c_posix_lock")
        tests.append("filesystem_tests::actual_f_import_alias_read_and_lock_custody_preserve_c_posix_sql_lock")
        tests.extend([
            "filesystem::preview_tests::actual_g_owns_backup_siblings_and_close_waits_for_checked_drain",
            "filesystem::preview_tests::actual_c_death_cancels_and_reaps_g_owned_backup_siblings",
            "filesystem::preview_tests::actual_public_backup_cancel_reaps_both_g_children_before_terminal_status",
            "filesystem::preview_tests::actual_global_inspect_and_restore_remain_legal_after_catalog_close",
        ])
    for test in tests:
        name = f"application::desktop::{test}"
        command = ["cargo", "test", "--locked", "--lib"]
        if args.release:
            command.append("--release")
        command += [name, "--", "--ignored", "--exact", "--nocapture", "--test-threads=1"]
        print(f"Running {name}", flush=True)
        result = subprocess.run(
            command, cwd=repository, env=environment,
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
        )
        print(result.stdout, end="", flush=True)
        if result.returncode:
            return result.returncode
        if "test result: ok. 1 passed; 0 failed;" not in result.stdout:
            print(f"Expected exactly one passing fixture: {name}", file=sys.stderr)
            return 1
    print(f"Verified {len(tests)} configured catalog/filesystem process fixtures.", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
