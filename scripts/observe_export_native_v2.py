#!/usr/bin/env python3
"""Observe one already-authorized managed export without starting or controlling it.

The catalog-wide photo-export.lock is deliberately ignored: the managed executor
can retain it while idle.  Evidence is admitted only for an exact native export
child, its bounded request, and its per-stage active.lock lease.
"""

from __future__ import annotations

import argparse
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import time
from typing import Any

import psutil


REQUEST_LIMIT = 256 * 1024
ROLE = "--photo-export-worker"
CLOCK = "macos_mach_absolute_ns"

DENIAL_RECHECK_NS = 250_000_000
DENIAL_RECHECK_INTERVAL_NS = 25_000_000
DENIAL_RECHECK_MAX_PROBES = 11


def exception_evidence(error: BaseException) -> list[dict[str, Any]]:
    """Keep bounded original errno/cause evidence, not only psutil's wrapper text."""
    chain = []
    seen = set()
    while error is not None and id(error) not in seen and len(chain) < 4:
        seen.add(id(error))
        chain.append({"type": type(error).__name__, "message": str(error)[:1024],
                      "errno": getattr(error, "errno", None)})
        error = error.__cause__ or error.__context__
    return chain


def lifecycle_observation(pid: int, birth: float | None) -> dict[str, Any]:
    observed: dict[str, Any] = {"observed_status": None, "observed_birth_unix_s": None, "exception_chain": []}
    operation = "process"
    try:
        process = psutil.Process(pid)
        operation = "status"
        observed["observed_status"] = process.status()
        if observed["observed_status"] == psutil.STATUS_ZOMBIE:
            return {**observed, "state": "gone", "detail": "zombie"}
        operation = "create_time"
        observed["observed_birth_unix_s"] = process.create_time()
        if birth is not None and observed["observed_birth_unix_s"] != birth:
            return {**observed, "state": "gone", "detail": "pid_reused"}
        return {**observed, "state": "live", "detail": "same_birth" if birth is not None else "unbound_live"}
    except (psutil.NoSuchProcess, psutil.ZombieProcess) as error:
        return {**observed, "state": "gone", "detail": "absent_or_zombie", "operation": operation,
                "exception_chain": exception_evidence(error)}
    except (psutil.AccessDenied, OSError) as error:
        return {**observed, "state": "unknown", "detail": f"{type(error).__name__}: {error}",
                "operation": operation, "exception_chain": exception_evidence(error)}


def lifecycle(pid: int, birth: float | None) -> tuple[str, str]:
    """Unknown permissions are never evidence of exit."""
    observation = lifecycle_observation(pid, birth)
    return observation["state"], observation["detail"]


def recheck_denied_lifecycle(pid: int, birth: float | None, denial_ns: int,
                             observer_deadline_ns: int) -> dict[str, Any]:
    """Read-only evidence after segment retirement; late results never excuse denial.

    The deadline caps scheduling and evidence admission. A synchronous kernel read
    may itself overrun; record that overrun and fail closed, without more probes.
    """
    deadline = min(denial_ns + DENIAL_RECHECK_NS, observer_deadline_ns)
    probes: list[dict[str, Any]] = []
    state, detail = "unknown", "recheck_budget_expired"
    for _ in range(DENIAL_RECHECK_MAX_PROBES):
        before_mono, before_wall = time.monotonic_ns(), time.time_ns()
        if before_mono >= deadline:
            break
        value = lifecycle_observation(pid, birth)
        after_mono, after_wall = time.monotonic_ns(), time.time_ns()
        within = after_mono <= deadline
        probes.append({"before_monotonic_ns": before_mono, "after_monotonic_ns": after_mono,
                       "before_wall_ns": before_wall, "after_wall_ns": after_wall,
                       "within_budget": within, **value})
        state, detail = value["state"], value["detail"]
        if not within:
            state, detail = "unknown", "lifecycle_read_exceeded_recheck_deadline"
            break
        if state == "gone":
            break
        remaining = deadline - time.monotonic_ns()
        if remaining <= 0:
            break
        time.sleep(min(DENIAL_RECHECK_INTERVAL_NS, remaining) / 1e9)
    return {"lifecycle": state, "lifecycle_detail": detail, "recheck_deadline_monotonic_ns": deadline,
            "recheck_budget_ns": DENIAL_RECHECK_NS, "recheck_max_probes": DENIAL_RECHECK_MAX_PROBES,
            "lifecycle_probes": probes}



def sha256_path(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb", buffering=1024 * 1024) as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def same_birth(process: psutil.Process | int, birth: float) -> bool:
    try:
        pid = process if isinstance(process, int) else process.pid
        fresh = psutil.Process(pid)
        return fresh.is_running() and fresh.status() != psutil.STATUS_ZOMBIE and fresh.create_time() == birth
    except (psutil.NoSuchProcess, psutil.ZombieProcess):
        return False


def ancestry(child: psutil.Process, root_pid: int, root_birth: float) -> list[dict[str, Any]]:
    chain: list[dict[str, Any]] = []
    current = psutil.Process(child.pid)
    for _ in range(64):
        born = current.create_time()
        chain.append({"pid": current.pid, "birth_unix_s": born})
        if current.pid == root_pid:
            if born != root_birth:
                raise RuntimeError("root PID birth changed")
            return chain
        parent_pid = current.ppid()
        if parent_pid <= 0:
            break
        current = psutil.Process(parent_pid)
    raise RuntimeError("native worker is not a descendant of the bound root")


def read_stage(cwd: Path, expected_workers: Path, expected_job: str) -> dict[str, Any]:
    if cwd.name == "" or not cwd.name.startswith("photo-worker-"):
        raise RuntimeError("worker cwd is not a photo-worker stage")
    if cwd.parent != expected_workers:
        raise RuntimeError("worker cwd is outside the bound export-workers directory")
    flags = os.O_RDONLY | os.O_NOFOLLOW
    directory_fd = os.open(cwd, flags | os.O_DIRECTORY)
    request_fd = None
    lock_fd = None
    try:
        directory_stat = os.fstat(directory_fd)
        path_stat = os.stat(cwd, follow_symlinks=False)
        if not stat.S_ISDIR(directory_stat.st_mode):
            raise RuntimeError("worker cwd handle is not a directory")
        if (directory_stat.st_dev, directory_stat.st_ino) != (path_stat.st_dev, path_stat.st_ino):
            raise RuntimeError("worker cwd identity changed")
        request_fd = os.open("request.json", flags, dir_fd=directory_fd)
        request_before = os.fstat(request_fd)
        if not stat.S_ISREG(request_before.st_mode) or request_before.st_size > REQUEST_LIMIT:
            raise RuntimeError("request.json size/type admission failed")
        chunks: list[bytes] = []
        remaining = request_before.st_size
        while remaining:
            chunk = os.read(request_fd, min(65536, remaining))
            if not chunk:
                raise RuntimeError("request.json ended before its admitted length")
            chunks.append(chunk)
            remaining -= len(chunk)
        if os.read(request_fd, 1):
            raise RuntimeError("request.json exceeded its admitted length")
        request_after = os.fstat(request_fd)
        stable_fields = ("st_dev", "st_ino", "st_size", "st_mtime_ns", "st_ctime_ns")
        if any(getattr(request_before, field) != getattr(request_after, field) for field in stable_fields):
            raise RuntimeError("request.json changed while read")
        request_bytes = b"".join(chunks)
        request = json.loads(request_bytes)
        if not isinstance(request, dict) or request.get("version") != 2:
            raise RuntimeError("managed request protocol differs from version 2")
        work = request.get("work")
        if not isinstance(work, dict) or work.get("job") != expected_job:
            raise RuntimeError("request.json job differs from the expected durable job")
        sequence = work.get("sequence")
        attempt = work.get("attempt")
        authority = work.get("authority")
        if isinstance(sequence, bool) or not isinstance(sequence, int) or sequence < 0:
            raise RuntimeError("request sequence admission failed")
        if not isinstance(attempt, str) or not 1 <= len(attempt.encode("utf-8")) <= 256:
            raise RuntimeError("request attempt admission failed")
        if (
            not isinstance(authority, str) or len(authority) != 64
            or any(character not in "0123456789abcdef" for character in authority)
        ):
            raise RuntimeError("request authority admission failed")
        lock_fd = os.open("active.lock", flags, dir_fd=directory_fd)
        lock_stat = os.fstat(lock_fd)
        if not stat.S_ISREG(lock_stat.st_mode):
            raise RuntimeError("active.lock is not a regular file")
        return {
            "cwd": str(cwd),
            "directory_device_inode": [directory_stat.st_dev, directory_stat.st_ino],
            "request_device_inode": [request_after.st_dev, request_after.st_ino],
            "request_bytes": len(request_bytes),
            "request_sha256": hashlib.sha256(request_bytes).hexdigest(),
            "active_lock": str(cwd / "active.lock"),
            "active_lock_device_inode": [lock_stat.st_dev, lock_stat.st_ino],
            "job": expected_job,
            "sequence": sequence,
            "attempt": attempt,
            "authority": authority,
        }
    finally:
        if lock_fd is not None:
            os.close(lock_fd)
        if request_fd is not None:
            os.close(request_fd)
        os.close(directory_fd)


def lock_contended(path: Path, expected_identity: tuple[int, int]) -> bool:
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    try:
        current = os.fstat(fd)
        if not stat.S_ISREG(current.st_mode):
            raise RuntimeError("active.lock changed type")
        if (current.st_dev, current.st_ino) != expected_identity:
            raise RuntimeError("active.lock identity changed")
        try:
            fcntl.flock(fd, fcntl.LOCK_SH | fcntl.LOCK_NB)
        except OSError as error:
            if error.errno in (errno.EAGAIN, errno.EACCES, errno.EWOULDBLOCK):
                return True
            raise
        else:
            fcntl.flock(fd, fcntl.LOCK_UN)
            return False
    finally:
        os.close(fd)


def child_holds_lock(pid: int, active_lock: Path) -> dict[str, Any]:
    started = time.monotonic_ns()
    result = subprocess.run(
        ["/usr/sbin/lsof", "-nP", "-Fpn", "-a", "-p", str(pid), "--", str(active_lock)],
        check=False,
        capture_output=True,
        text=True,
        timeout=1,
    )
    elapsed = time.monotonic_ns() - started
    names = [line[1:] for line in result.stdout.splitlines() if line.startswith("n")]
    exact = any(Path(name) == active_lock for name in names)
    return {
        "exit": result.returncode,
        "exact_path_open": exact,
        "names": names[:8],
        "stderr": result.stderr[:1024],
        "elapsed_ns": elapsed,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--pid", type=int, required=True, help="installed GUI root PID")
    parser.add_argument("--exe", type=Path, required=True, help="exact installed executable")
    parser.add_argument("--exe-sha256", required=True)
    parser.add_argument("--export-workers", type=Path, required=True)
    parser.add_argument("--job", required=True, help="predeclared durable export job UUID")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--seconds", type=float, default=300.0)
    parser.add_argument("--interval", type=float, default=0.05)
    args = parser.parse_args()
    if not 0 < args.seconds <= 300:
        parser.error("seconds must be in (0, 300]")
    if not 0.025 <= args.interval <= 0.25:
        parser.error("interval must be in [0.025, 0.25]")
    if not 1 <= len(args.job.encode("utf-8")) <= 256:
        parser.error("job must contain 1..256 UTF-8 bytes")
    expected_hash = args.exe_sha256.lower()
    if len(expected_hash) != 64 or any(c not in "0123456789abcdef" for c in expected_hash):
        parser.error("exe-sha256 must be 64 lowercase hexadecimal characters")

    executable = args.exe.resolve(strict=True)

    def executable_identity() -> dict[str, Any]:
        value = executable.stat()
        if not stat.S_ISREG(value.st_mode):
            raise RuntimeError("bound executable is not a regular file")
        return {
            "device": value.st_dev, "inode": value.st_ino, "bytes": value.st_size,
            "mode": stat.S_IMODE(value.st_mode), "mtime_ns": value.st_mtime_ns,
            "ctime_ns": value.st_ctime_ns,
        }

    executable_before = executable_identity()
    observed_hash = sha256_path(executable)
    if executable_identity() != executable_before or observed_hash != expected_hash:
        raise RuntimeError("installed executable identity/hash mismatch before observation")
    if not args.export_workers.is_absolute() or args.export_workers.name != "export-workers":
        raise RuntimeError("export-workers must be an absolute path ending in export-workers")
    workers_parent = args.export_workers.parent.resolve(strict=True)
    workers = workers_parent / "export-workers"
    if workers.exists() and (not workers.is_dir() or workers.resolve(strict=True) != workers):
        raise RuntimeError("existing export-workers path is not the exact canonical directory")
    root = psutil.Process(args.pid)
    root_birth = root.create_time()
    root_exe = Path(root.exe()).resolve(strict=True)
    if root_exe != executable:
        raise RuntimeError("root process executable differs from the bound installed executable")
    if args.output.exists():
        raise RuntimeError("output already exists")

    if sys.platform != "darwin" or time.get_clock_info("monotonic").implementation != "mach_absolute_time()":
        raise RuntimeError("observer v2 requires the qualified macOS absolute clock")

    retired: set[tuple[int, float]] = set()
    admitted: dict[tuple[int, float], dict[str, Any]] = {}
    segments: list[dict[str, Any]] = []
    current_segment: dict[tuple[int, float], int] = {}
    present_previous: set[tuple[int, float]] = set()
    fatal_errors = 0
    observation_gaps = 0
    positive_observations = 0
    deadline_ns = time.monotonic_ns() + round(args.seconds * 1e9)
    deadline = time.monotonic() + args.seconds
    initial_wall = time.time_ns()
    initial_mono = time.monotonic_ns()
    initial_clock_offset = initial_wall - initial_mono
    max_clock_offset_drift_ns = 0
    clock_observations = 0

    with args.output.open("x", encoding="utf-8") as output:
        def emit(value: dict[str, Any]) -> None:
            nonlocal max_clock_offset_drift_ns, clock_observations
            for wall_key, monotonic_key in (
                ("wall_ns", "monotonic_ns"),
                ("positive_before_wall_ns", "positive_before_monotonic_ns"),
                ("positive_after_wall_ns", "positive_after_monotonic_ns"),
            ):
                wall = value.get(wall_key)
                monotonic = value.get(monotonic_key)
                if isinstance(wall, int) and isinstance(monotonic, int):
                    drift = abs((wall - monotonic) - initial_clock_offset)
                    max_clock_offset_drift_ns = max(max_clock_offset_drift_ns, drift)
                    clock_observations += 1
            output.write(json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n")
            output.flush()

        emit({
            "kind": "identity",
            "protocol": 2, "clock": CLOCK,
            "python_version": sys.version, "psutil_version": psutil.__version__,
            "root_pid": args.pid,
            "root_birth_unix_s": root_birth,
            "executable": str(executable),
            "executable_sha256": observed_hash,
            "executable_identity": executable_before,
            "export_workers": str(workers),
            "job": args.job,
            "seconds": args.seconds,
            "interval": args.interval,
            "wall_ns": initial_wall,
            "monotonic_ns": initial_mono,
            "warning": "Observation only; this process never starts, signals, or controls the export.",
        })
        def close_segment(key: tuple[int, float], reason: str, wall_ns: int, monotonic_ns: int) -> None:
            retired.add(key)
            index = current_segment.pop(key, None)
            if index is None:
                return
            segment = segments[index]
            segment["closed_reason"] = reason
            segment["closed_observed_wall_ns"] = wall_ns
            segment["closed_observed_monotonic_ns"] = monotonic_ns
            emit({"kind": "segment_closed", "segment": segment["segment"],
                  "pid": key[0], "birth_unix_s": key[1], "closed_reason": reason,
                  "wall_ns": wall_ns, "monotonic_ns": monotonic_ns})

        def start_segment(
            key: tuple[int, float], stage: dict[str, Any],
            positive_before_wall_ns: int, positive_before_monotonic_ns: int,
            positive_after_wall_ns: int, positive_after_monotonic_ns: int,
            lsof_confirmed: bool,
        ) -> int:
            segment = {
                "segment": len(segments) + 1,
                "pid": key[0], "birth_unix_s": key[1],
                # The lease is proved only after the first successful observation.
                "first_positive_after_wall_ns": positive_after_wall_ns,
                "first_positive_after_monotonic_ns": positive_after_monotonic_ns,
                # A usable end exists only before a later successful observation.
                "last_positive_before_wall_ns": None,
                "last_positive_before_monotonic_ns": None,
                "last_positive_after_wall_ns": positive_after_wall_ns,
                "last_positive_after_monotonic_ns": positive_after_monotonic_ns,
                "positive_observations": 1,
                "lsof_confirmations": 1 if lsof_confirmed else 0,
                "last_lsof_monotonic_ns": positive_after_monotonic_ns if lsof_confirmed else 0,
                "stage": stage,
            }
            segments.append(segment)
            current_segment[key] = len(segments) - 1
            return len(segments) - 1

        while time.monotonic() < deadline:
            root_state, root_detail = lifecycle(args.pid, root_birth)
            if root_state != "live":
                fatal_errors += 1
                emit({"kind": "error", "operation": "root_lifecycle", "pid": args.pid,
                      "wall_ns": time.time_ns(), "monotonic_ns": time.monotonic_ns(),
                      "error": root_detail, "lifecycle": root_state})
                break
            loop_started = time.monotonic_ns()
            wall_ns = time.time_ns()
            try:
                children = root.children(recursive=True)
            except (psutil.NoSuchProcess, psutil.ZombieProcess, psutil.AccessDenied, OSError) as error:
                fatal_errors += 1
                emit({"kind": "error", "operation": "root_children", "pid": args.pid,
                      "wall_ns": time.time_ns(), "monotonic_ns": time.monotonic_ns(),
                      "error": f"{type(error).__name__}: {error}"})
                break
            seen: set[tuple[int, float]] = set()
            for listed_child in children:
                key: tuple[int, float] | None = None
                operation = "child_identity"
                try:
                    child = psutil.Process(listed_child.pid)
                    birth = child.create_time()
                    key = (child.pid, birth)
                    if key in retired:
                        continue
                    operation = "cmdline"
                    cmdline = child.cmdline()
                    if cmdline[1:] != [ROLE]:
                        continue
                    seen.add(key)
                    operation = "executable"
                    child_exe = Path(child.exe()).resolve(strict=True)
                    if child_exe != executable:
                        raise RuntimeError("worker executable path differs from bound executable")
                    operation = "cwd"
                    cwd = Path(child.cwd()).resolve(strict=True)
                    operation = "ancestry"
                    chain = ancestry(child, args.pid, root_birth)
                    if key not in admitted:
                        admission_before_wall = time.time_ns()
                        admission_before_mono = time.monotonic_ns()
                        operation = "stage_admission"
                        stage = read_stage(cwd, workers, args.job)
                        initial_contended = lock_contended(
                            Path(stage["active_lock"]), tuple(stage["active_lock_device_inode"])
                        )
                        operation = "lsof"
                        lsof = child_holds_lock(child.pid, Path(stage["active_lock"]))
                        birth_current = same_birth(child.pid, birth)
                        final_contended = lock_contended(
                            Path(stage["active_lock"]), tuple(stage["active_lock_device_inode"])
                        )
                        admission_after_wall = time.time_ns()
                        admission_after_mono = time.monotonic_ns()
                        if (
                            not initial_contended or not final_contended or not birth_current
                            or not lsof["exact_path_open"] or lsof["elapsed_ns"] > 1_100_000_000
                        ):
                            emit({"kind": "candidate_rejected", "pid": child.pid,
                                  "birth_unix_s": birth,
                                  "positive_before_wall_ns": admission_before_wall,
                                  "positive_before_monotonic_ns": admission_before_mono,
                                  "positive_after_wall_ns": admission_after_wall,
                                  "positive_after_monotonic_ns": admission_after_mono,
                                  "initial_contended": initial_contended,
                                  "final_contended": final_contended,
                                  "birth_current_after_lsof": birth_current, "lsof": lsof})
                            continue
                        stage["ancestry_child_to_root"] = chain
                        stage["lsof_admission"] = lsof
                        admitted[key] = stage
                        segment_index = start_segment(
                            key, stage, admission_before_wall, admission_before_mono,
                            admission_after_wall, admission_after_mono, True,
                        )
                        positive_observations += 1
                        emit({"kind": "stage_admitted", "pid": child.pid,
                              "birth_unix_s": birth, "segment": segment_index + 1,
                              "positive_before_wall_ns": admission_before_wall,
                              "positive_before_monotonic_ns": admission_before_mono,
                              "positive_after_wall_ns": admission_after_wall,
                              "positive_after_monotonic_ns": admission_after_mono, **stage})
                        continue
                    stage = admitted[key]
                    operation = "active_probe"
                    probe_before_wall = time.time_ns()
                    probe_before_mono = time.monotonic_ns()
                    if not same_birth(child.pid, birth):
                        observation_gaps += 1
                        close_segment(key, "pid_birth_not_current", probe_before_wall, probe_before_mono)
                        emit({"kind": "observation_gap", "pid": child.pid,
                              "birth_unix_s": birth, "reason": "pid_birth_not_current",
                              "wall_ns": probe_before_wall, "monotonic_ns": probe_before_mono})
                        continue
                    contended = lock_contended(
                        Path(stage["active_lock"]), tuple(stage["active_lock_device_inode"])
                    )
                    if not contended:
                        observation_gaps += 1
                        close_segment(key, "active_lock_not_contended", probe_before_wall, probe_before_mono)
                        emit({"kind": "observation_gap", "pid": child.pid,
                              "birth_unix_s": birth, "reason": "active_lock_not_contended",
                              "wall_ns": probe_before_wall, "monotonic_ns": probe_before_mono})
                        continue
                    lsof_confirmed = False
                    segment_index = current_segment.get(key)
                    last_lsof = segments[segment_index]["last_lsof_monotonic_ns"] if segment_index is not None else 0
                    if probe_before_mono - last_lsof >= 2_000_000_000:
                        operation = "lsof"
                        lsof = child_holds_lock(child.pid, Path(stage["active_lock"]))
                        if not same_birth(child.pid, birth):
                            observation_gaps += 1
                            close_segment(key, "pid_birth_changed_after_lsof", time.time_ns(), time.monotonic_ns())
                            emit({"kind": "observation_gap", "pid": child.pid,
                                  "birth_unix_s": birth, "reason": "pid_birth_changed_after_lsof",
                                  "wall_ns": time.time_ns(), "monotonic_ns": time.monotonic_ns(),
                                      "lsof": lsof})
                            continue
                        if not lsof["exact_path_open"] or lsof["elapsed_ns"] > 1_100_000_000:
                            raise RuntimeError("live worker active.lock lsof admission failed")
                        if not lock_contended(
                            Path(stage["active_lock"]), tuple(stage["active_lock_device_inode"])
                        ):
                            observation_gaps += 1
                            close_segment(key, "active_lock_changed_after_lsof", time.time_ns(), time.monotonic_ns())
                            emit({"kind": "observation_gap", "pid": child.pid,
                                  "birth_unix_s": birth, "reason": "active_lock_changed_after_lsof",
                                  "wall_ns": time.time_ns(), "monotonic_ns": time.monotonic_ns(),
                                  "lsof": lsof})
                            continue
                        lsof_confirmed = True
                    elif not same_birth(child.pid, birth):
                        observation_gaps += 1
                        close_segment(key, "pid_birth_changed_after_probe", time.time_ns(), time.monotonic_ns())
                        emit({"kind": "observation_gap", "pid": child.pid,
                              "birth_unix_s": birth, "reason": "pid_birth_changed_after_probe",
                              "wall_ns": time.time_ns(), "monotonic_ns": time.monotonic_ns()})
                        continue
                    probe_after_wall = time.time_ns()
                    probe_after_mono = time.monotonic_ns()
                    segment_index = current_segment.get(key)
                    if segment_index is None:
                        segment_index = start_segment(
                            key, stage, probe_before_wall, probe_before_mono,
                            probe_after_wall, probe_after_mono, lsof_confirmed,
                        )
                    else:
                        segment = segments[segment_index]
                        segment["last_positive_before_wall_ns"] = probe_before_wall
                        segment["last_positive_before_monotonic_ns"] = probe_before_mono
                        segment["last_positive_after_wall_ns"] = probe_after_wall
                        segment["last_positive_after_monotonic_ns"] = probe_after_mono
                        segment["positive_observations"] += 1
                        if lsof_confirmed:
                            segment["lsof_confirmations"] += 1
                            segment["last_lsof_monotonic_ns"] = probe_after_mono
                    positive_observations += 1
                    emit({"kind": "active", "pid": child.pid, "birth_unix_s": birth,
                          "segment": segment_index + 1,
                          "positive_before_wall_ns": probe_before_wall,
                          "positive_before_monotonic_ns": probe_before_mono,
                          "positive_after_wall_ns": probe_after_wall,
                          "positive_after_monotonic_ns": probe_after_mono,
                          "lsof_rechecked": lsof_confirmed,
                          "job": stage["job"], "sequence": stage["sequence"],
                          "attempt": stage["attempt"], "authority": stage["authority"],
                          "active_lock_device_inode": stage["active_lock_device_inode"]})
                except (psutil.NoSuchProcess, psutil.ZombieProcess, FileNotFoundError) as error:
                    observation_gaps += 1
                    now_wall, now_mono = time.time_ns(), time.monotonic_ns()
                    if key is not None:
                        close_segment(key, "lifecycle_race", now_wall, now_mono)
                    emit({"kind": "observation_gap", "pid": listed_child.pid,
                          "wall_ns": now_wall, "monotonic_ns": now_mono,
                          "operation": operation, "reason": f"{type(error).__name__}: {error}"})
                except psutil.AccessDenied as error:
                    # Retire at the first denial, before inspection or any wait.
                    denial_mono, denial_wall = time.monotonic_ns(), time.time_ns()
                    if key is not None:
                        close_segment(key, "denied_access", denial_wall, denial_mono)
                    evidence = recheck_denied_lifecycle(listed_child.pid, key[1] if key else None,
                                                        denial_mono, deadline_ns)
                    if evidence["lifecycle"] == "gone":
                        observation_gaps += 1
                    else:
                        fatal_errors += 1
                    emit({"kind": "observation_gap" if evidence["lifecycle"] == "gone" else "error",
                          "pid": listed_child.pid, "birth_unix_s": key[1] if key else None,
                          "operation": operation, "error": f"{type(error).__name__}: {error}",
                          "denial_monotonic_ns": denial_mono, "denial_wall_ns": denial_wall,
                          "exception_chain": exception_evidence(error), **evidence,
                          "wall_ns": time.time_ns(), "monotonic_ns": time.monotonic_ns()})
                except (OSError, ValueError, RuntimeError, json.JSONDecodeError,
                        subprocess.TimeoutExpired) as error:
                    fatal_errors += 1
                    now_wall, now_mono = time.time_ns(), time.monotonic_ns()
                    if key is not None:
                        close_segment(key, "fatal_observation_error", now_wall, now_mono)
                    emit({"kind": "error", "pid": listed_child.pid,
                          "wall_ns": time.time_ns(), "monotonic_ns": time.monotonic_ns(),
                          "operation": operation, "error": f"{type(error).__name__}: {error}"})
            for key in present_previous - seen:
                now_wall, now_mono = time.time_ns(), time.monotonic_ns()
                close_segment(key, "child_not_seen", now_wall, now_mono)
                if key in admitted:
                    stage = admitted[key]
                    emit({"kind": "stage_not_seen", "pid": key[0], "birth_unix_s": key[1],
                          "wall_ns": now_wall, "monotonic_ns": now_mono,
                          "job": stage["job"], "sequence": stage["sequence"],
                          "attempt": stage["attempt"], "authority": stage["authority"]})
            present_previous = seen
            elapsed = (time.monotonic_ns() - loop_started) / 1e9
            time.sleep(max(0.0, args.interval - elapsed))
        measurement_end_wall = time.time_ns()
        measurement_end_mono = time.monotonic_ns()
        for key in list(current_segment):
            close_segment(key, "observer_end", measurement_end_wall, measurement_end_mono)
        measurement_end_drift_ns = abs(
            (measurement_end_wall - measurement_end_mono) - initial_clock_offset
        )
        max_clock_offset_drift_ns = max(max_clock_offset_drift_ns, measurement_end_drift_ns)
        clock_observations += 1
        executable_final_error = None
        try:
            executable_final_hash = sha256_path(executable)
            executable_final_identity = executable_identity()
        except Exception as error:
            executable_final_hash = None
            executable_final_identity = None
            executable_final_error = f"{type(error).__name__}: {error}"
        executable_unchanged = (
            executable_final_error is None
            and executable_final_hash == observed_hash
            and executable_final_identity == executable_before
        )
        usable_segments = sum(
            segment["last_positive_before_monotonic_ns"] is not None
            and segment["last_positive_before_monotonic_ns"] >= segment["first_positive_after_monotonic_ns"]
            for segment in segments
        )
        emit({
            "kind": "summary",
            "protocol": 2, "clock": CLOCK,
            "wall_clock_drift_is_diagnostic_only": True,
            "measurement_end_wall_ns": measurement_end_wall,
            "measurement_end_monotonic_ns": measurement_end_mono,
            "final_validation_wall_ns": time.time_ns(),
            "max_clock_offset_drift_ns": max_clock_offset_drift_ns,
            "clock_observations": clock_observations,
            "clock_offset_within_5ms": max_clock_offset_drift_ns <= 5_000_000,
            "root_same_birth": lifecycle(args.pid, root_birth)[0] == "live",
            "admitted_stages": len(admitted),
            "positive_observations": positive_observations,
            "usable_segments": usable_segments,
            "observation_gaps": observation_gaps,
            "fatal_errors": fatal_errors,
            "executable_final_identity": executable_final_identity,
            "executable_final_sha256": executable_final_hash,
            "executable_final_error": executable_final_error,
            "executable_unchanged": executable_unchanged,
            "segments": segments,
        })
    return 0 if usable_segments and fatal_errors == 0 and executable_unchanged and lifecycle(args.pid, root_birth)[0] == "live" else 2


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        raise SystemExit(130)
    except Exception as error:
        print(f"observe-export-native: {type(error).__name__}: {error}", file=sys.stderr)
        raise SystemExit(2)
