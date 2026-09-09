"""Bind passive host telemetry to a preview campaign; never judge quietness."""
from __future__ import annotations
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import subprocess
import threading
import time
import psutil

OBSERVER = Path(__file__).resolve().parents[1] / "benchmarks" / "observe_host.py"
spec = importlib.util.spec_from_file_location("preview_passive_observer", OBSERVER)
observer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(observer)


def sha(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def storage(path):
    path = Path(path).resolve()
    result = {"path": str(path), "capacity": psutil.disk_usage(path)._asdict()}
    try:
        mounts = [p for p in psutil.disk_partitions(all=True)
                  if path == Path(p.mountpoint) or Path(p.mountpoint) in path.parents]
        mount = max(mounts, key=lambda p: len(p.mountpoint))
        result.update(status="available", filesystem=mount.fstype,
                      device_basename=Path(mount.device).name, mountpoint=mount.mountpoint)
    except (ValueError, OSError, psutil.Error) as error:
        result.update(status="unavailable", reason=type(error).__name__)
    return result


def host_identity(output, sources):
    cpu = {"status": "unavailable", "model": None}
    if platform.system() == "Darwin":
        try:
            value = subprocess.run(["/usr/sbin/sysctl", "-n", "machdep.cpu.brand_string"],
                                   capture_output=True, text=True, timeout=3, check=True)
            cpu = {"status": "available", "model": value.stdout.strip()}
        except (OSError, subprocess.SubprocessError) as error:
            cpu["reason"] = type(error).__name__
    return {"system": platform.system(), "release": platform.release(),
            "machine": platform.machine(), "cpu": cpu, "cpus": psutil.cpu_count(),
            "ram_bytes": psutil.virtual_memory().total, "psutil_version": psutil.__version__,
            "output_storage": storage(output),
            "input_storage": [storage(p) for p in sorted({str(Path(p).resolve().parent) for p in sources})],
            "telemetry": {"path": "host.jsonl", "receipt": "host-receipt.json",
                          "interval_seconds": 1, "observer_sha256": sha(OBSERVER),
                          "binding_sha256": sha(__file__), "judges_quietness": False}}


class HostObservation:
    """Every sample uses the reviewed observer: no args, environment or serials."""
    def __init__(self, output):
        self.output = Path(output)
        self.stop = threading.Event()
        self.error = None
        self.samples = 0
        self.finished = None
        self.started = time.monotonic()
        self.thread = None
        self.stream = None

    def write(self, value):
        self.stream.write(json.dumps(value, allow_nan=False) + "\n")
        self.stream.flush()

    def __enter__(self):
        self.stream = (self.output / "host.jsonl").open("x", encoding="utf-8")
        self.write({"kind": "start", **observer.stamp(), "interval_seconds": 1,
                    "observer_sha256": sha(OBSERVER), "auxiliary_telemetry_only": True})
        self.sampler = observer.HostSampler()
        # A real sample is persisted before the first measured child is admitted.
        try:
            self.write(self.sampler.sample())
            self.samples += 1
        except BaseException:
            self.stream.close()
            raise
        self.thread = threading.Thread(target=self.collect, name="preview-host-observer", daemon=True)
        self.thread.start()
        return self

    def collect(self):
        try:
            while not self.stop.wait(1):
                self.write(self.sampler.sample())
                self.samples += 1
        except Exception as error:
            self.error = type(error).__name__

    def finish(self):
        if self.finished is not None:
            return self.finished
        self.stop.set()
        if self.thread:
            self.thread.join(timeout=10)
            if self.thread.is_alive():
                raise RuntimeError("host observation failed to stop; campaign is incomplete")
        self.write({"kind": "final", **observer.stamp(), "samples": self.samples,
                    "error": self.error, "elapsed_seconds": time.monotonic() - self.started})
        self.stream.flush()
        os.fsync(self.stream.fileno())
        self.stream.close()
        self.finished = {"complete": self.error is None and self.samples > 0,
                         "samples": self.samples, "error": self.error,
                         "sha256": sha(self.output / "host.jsonl"),
                         "observer_sha256": sha(OBSERVER), "binding_sha256": sha(__file__),
                         "quietness": "not evaluated; unavailable fields never mean idle"}
        with (self.output / "host-receipt.json").open("x", encoding="utf-8") as stream:
            json.dump(self.finished, stream, indent=2, allow_nan=False)
            stream.flush()
            os.fsync(stream.fileno())
        return self.finished

    def __exit__(self, *_args):
        self.finish()
