#!/usr/bin/env python3
"""Run one actual native cache regression in an owned private mount namespace.

No simulation or successful skip: Linux, statx mount IDs, sudo and mount capability
are required. Cargo runs unprivileged; only the exact ignored test runs as root.
"""
import json
import os
from pathlib import Path
import subprocess
import sys

TEST = "import_storage::linux_mount_tests::same_device_file_bind_mount_bypasses_parent_cache"
ROOT = Path(__file__).resolve().parents[1]


def main():
    if sys.platform != "linux":
        raise SystemExit("This validation requires native Linux")
    build = subprocess.run(
        ["cargo", "test", "--locked", "--lib", "--no-run", "--message-format=json"],
        cwd=ROOT, check=True, text=True, stdout=subprocess.PIPE,
    )
    artifacts = set()
    for line in build.stdout.splitlines():
        item = json.loads(line)
        if (item.get("reason") == "compiler-artifact"
                and item.get("target", {}).get("name") == "photocatalog"
                and "lib" in item["target"]["kind"]
                and item.get("profile", {}).get("test")
                and item.get("executable")):
            artifacts.add(Path(item["executable"]).resolve(strict=True))
    if len(artifacts) != 1:
        raise SystemExit(f"Expected exactly one native library test binary, found {len(artifacts)}")
    binary = artifacts.pop()
    listing = subprocess.run(
        [str(binary), "--list", "--exact", TEST],
        check=True, text=True, stdout=subprocess.PIPE,
    ).stdout
    if f"{TEST}: test" not in listing.splitlines():
        raise SystemExit("Exact native mount regression absent; refusing a zero-test pass")
    # Environment is passed as argv, with no shell expansion. Native DNG/JXL
    # shared libraries may live outside the hosted runner's system linker path.
    command = [
        "sudo", "-n", "--", "env",
        f"LD_LIBRARY_PATH={os.environ.get('LD_LIBRARY_PATH', '')}",
        f"PHOTOCATALOG_OUTER_MOUNT_NAMESPACE={os.readlink('/proc/self/ns/mnt')}",
        "timeout", "--signal=TERM", "--kill-after=5s", "60s",
        "unshare", "--mount", "--propagation", "private", "--",
        str(binary), "--ignored", "--exact", TEST, "--nocapture", "--test-threads=1",
    ]
    print(f"Executing exact native Linux test: {TEST}", flush=True)
    subprocess.run(command, cwd=ROOT, check=True)


if __name__ == "__main__":
    main()
