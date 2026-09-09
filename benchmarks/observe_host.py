#!/usr/bin/env python3
"""Passive, auxiliary host telemetry. Never controls work or judges benchmark eligibility."""
import argparse
import datetime
import json
import math
import os
import platform
import plistlib
import signal
import subprocess
import threading
import time

import psutil


GPU_FIELDS = {
    "device_utilization_percent": "Device Utilization %",
    "renderer_utilization_percent": "Renderer Utilization %",
    "tiler_utilization_percent": "Tiler Utilization %",
    "allocated_system_memory_bytes": "Alloc system memory",
    "in_use_system_memory_bytes": "In use system memory",
}
DISK_FIELDS = ("read_count", "write_count", "read_bytes", "write_bytes", "read_time", "write_time")


def stamp():
    return {"utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
            "monotonic_seconds": time.monotonic()}


def counter_delta(current, previous):
    """No invented zero for a first sample, missing counters, or counter reset."""
    if current is None:
        return {"status": "unavailable", "values": None}
    if previous is None:
        return {"status": "baseline", "values": None}
    if current.keys() != previous.keys():
        return {"status": "unavailable", "reason": "counter_set_changed", "values": None}
    decreased = [key for key in current if current[key] < previous[key]]
    if decreased:
        return {"status": "reset", "decreased_counters": decreased, "values": None}
    return {"status": "available", "values": {key: current[key] - previous[key] for key in current}}


def gpu_snapshot(system=None, runner=subprocess.run):
    if (system or platform.system()) != "Darwin":
        return {"status": "unavailable", "reason": "AGX_ioreg_requires_macos", "devices": []}
    try:
        result = runner(["/usr/sbin/ioreg", "-a", "-l", "-r", "-c", "AGXAccelerator"],
                        capture_output=True, timeout=3, check=False)
        if result.returncode:
            return {"status": "error", "reason": "ioreg_nonzero_exit",
                    "exit_code": result.returncode, "devices": []}
        tree = plistlib.loads(result.stdout)
        devices = []

        def visit(node):
            if isinstance(node, list):
                for item in node:
                    visit(item)
            elif isinstance(node, dict):
                stats = node.get("PerformanceStatistics")
                if isinstance(stats, dict):
                    values = {}
                    for field, source in GPU_FIELDS.items():
                        value = stats.get(source)
                        values[field] = (value if isinstance(value, (int, float))
                                         and not isinstance(value, bool) and math.isfinite(value) else None)
                    missing = [key for key, value in values.items() if value is None]
                    devices.append({"index": len(devices), "status": "partial" if missing else "available",
                                    "missing_fields": missing, **values})
                visit(node.get("IORegistryEntryChildren", []))

        visit(tree)
        if not devices:
            return {"status": "unavailable", "reason": "AGX_statistics_not_found", "devices": []}
        return {"status": "partial" if any(d["missing_fields"] for d in devices) else "available",
                "source": "ioreg_AGX_PerformanceStatistics", "devices": devices}
    except (OSError, subprocess.SubprocessError, ValueError, plistlib.InvalidFileException) as error:
        # Error messages/stderr may contain paths. Record only the exception class.
        return {"status": "error", "reason": type(error).__name__, "devices": []}


class HostSampler:
    def __init__(self):
        self.previous_disk = None
        self.previous_swap = None
        self.previous_processes = {}
        self.previous_monotonic = None
        self.cpu_primed = False

    def sample(self):
        record = {"kind": "sample", **stamp()}
        now = record["monotonic_seconds"]
        record["interval_seconds"] = None if self.previous_monotonic is None else now - self.previous_monotonic
        self.previous_monotonic = now
        try:
            percent = psutil.cpu_percent(interval=None)
            record["cpu"] = {"status": "available" if self.cpu_primed else "baseline",
                             "percent": percent if self.cpu_primed else None,
                             "logical_cpus": psutil.cpu_count()}
            self.cpu_primed = True
        except psutil.Error as error:
            record["cpu"] = {"status": "error", "reason": type(error).__name__, "percent": None}
            self.cpu_primed = False
        for name, function in (("ram", psutil.virtual_memory), ("swap", psutil.swap_memory)):
            try:
                record[name] = {"status": "available", **function()._asdict()}
                if name == "swap":
                    counters = {key: record[name][key] for key in ("sin", "sout")}
                    record[name]["io_delta_bytes"] = counter_delta(counters, self.previous_swap)
                    self.previous_swap = counters
            except (psutil.Error, OSError) as error:
                record[name] = {"status": "error", "reason": type(error).__name__}
                if name == "swap":
                    self.previous_swap = None
        try:
            disk = psutil.disk_io_counters(nowrap=False)
            counters = None if disk is None else {key: getattr(disk, key) for key in DISK_FIELDS}
            record["disk_io"] = {"status": "unavailable" if counters is None else "available",
                                 "cumulative": counters, "delta": counter_delta(counters, self.previous_disk)}
            self.previous_disk = counters
        except (psutil.Error, OSError) as error:
            record["disk_io"] = {"status": "error", "reason": type(error).__name__, "delta": None}
            self.previous_disk = None
        processes = []
        current_processes = {}
        disappeared = 0
        try:
            # psutil caches Process objects and create_time across process_iter
            # calls; discard them so a reused PID gets its current identity.
            psutil.process_iter.cache_clear()
            for process in psutil.process_iter():
                try:
                    # Do not request argv, executable path, environment, username or open files.
                    with process.oneshot():
                        name = process.name().replace("\\", "/").rsplit("/", 1)[-1]
                        created = process.create_time()
                        cpu = process.cpu_times()
                        rss = process.memory_info().rss
                    seconds = cpu.user + cpu.system
                    previous = self.previous_processes.get(process.pid)
                    percent = None
                    status = "baseline"
                    if previous and previous[0] == created:
                        elapsed = now - previous[2]
                        delta = seconds - previous[1]
                        if elapsed > 0 and delta >= 0:
                            percent = 100 * delta / elapsed
                            status = "available"
                        else:
                            status = "reset"
                    current_processes[process.pid] = (created, seconds, now)
                    processes.append({"pid": process.pid, "name": name, "cpu_percent": percent,
                                      "cpu_status": status, "rss_bytes": rss})
                except psutil.NoSuchProcess:
                    disappeared += 1
                except (psutil.AccessDenied, OSError) as error:
                    processes.append({"pid": process.pid, "name": None, "cpu_percent": None,
                                      "rss_bytes": None, "status": "unavailable", "reason": type(error).__name__})
            record["processes"] = {"status": "partial" if any("reason" in p for p in processes) else "available",
                                   "disappeared_during_sample": disappeared, "items": processes}
        except (psutil.Error, OSError) as error:
            record["processes"] = {"status": "error", "reason": type(error).__name__, "items": processes}
        self.previous_processes = current_processes
        record["gpu"] = gpu_snapshot()
        record["acquisition_seconds"] = time.monotonic() - now
        return record


def positive_number(value):
    number = float(value)
    if not math.isfinite(number) or number <= 0:
        raise argparse.ArgumentTypeError("must be finite and greater than zero")
    return number


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, help="New private JSONL file; parent directory must exist")
    parser.add_argument("--duration-seconds", required=True, type=positive_number)
    parser.add_argument("--interval-seconds", type=positive_number, default=1.0)
    args = parser.parse_args(argv)
    if args.interval_seconds < 0.1:
        parser.error("interval must be at least 0.1 seconds")
    # Exclusive creation prevents overwriting earlier evidence; permissions apply at creation.
    descriptor = os.open(args.output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    stopped = threading.Event()
    reason = "duration_elapsed"

    def stop(signum, _frame):
        nonlocal reason
        reason = signal.Signals(signum).name
        stopped.set()

    previous_handlers = {sig: signal.signal(sig, stop) for sig in (signal.SIGINT, signal.SIGTERM)}
    with os.fdopen(descriptor, "w", encoding="utf-8") as output:
        def write(record):
            output.write(json.dumps(record, allow_nan=False, separators=(",", ":")) + "\n")
            output.flush()
        start = time.monotonic()
        samples = 0
        try:
            write({"kind": "start", **stamp(), "schema": "photocatalog-host-observer-1",
                   "observer_pid": os.getpid(), "platform": platform.system(),
                   "duration_seconds": args.duration_seconds, "interval_seconds": args.interval_seconds,
                   "psutil_version": psutil.__version__, "auxiliary_telemetry_only": True})
            sampler = HostSampler()
            deadline = start + args.duration_seconds
            while not stopped.is_set() and time.monotonic() < deadline:
                sample_start = time.monotonic()
                write(sampler.sample())
                samples += 1
                delay = min(sample_start + args.interval_seconds, deadline) - time.monotonic()
                stopped.wait(max(0, delay))
        except Exception as error:
            reason = "observer_error"
            write({"kind": "error", **stamp(), "reason": type(error).__name__})
            raise
        finally:
            write({"kind": "final", **stamp(), "reason": reason, "samples": samples,
                   "elapsed_seconds": time.monotonic() - start})
            for sig, handler in previous_handlers.items():
                signal.signal(sig, handler)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
