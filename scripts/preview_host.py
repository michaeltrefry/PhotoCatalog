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
    def __init__(self, output, *, max_bytes=None, max_record_bytes=None):
        for value in (max_bytes,max_record_bytes):
            if value is not None and (type(value) is not int or value<=0):
                raise ValueError('positive host evidence bounds required')
        self.max_bytes=max_bytes
        self.max_record_bytes=max_record_bytes
        self.written_bytes=0
        self.output = Path(output)
        self.stop = threading.Event()
        self.error = None
        self.samples = 0
        self.finished = None
        self.started = time.monotonic()
        self.thread = None
        self.stream = None

    def write(self, value):
        # Construct at most one bounded record; never retain or silently truncate
        # an oversized sample. Existing non-editing callers retain prior defaults.
        encoded=bytearray()
        for part in json.JSONEncoder(allow_nan=False).iterencode(value):
            data=part.encode('utf-8')
            if self.max_record_bytes is not None and len(encoded)+len(data)+1>self.max_record_bytes:
                self.error='host record byte admission exceeded'
                raise RuntimeError(self.error)
            encoded.extend(data)
        encoded.extend(b'\n')
        if self.max_bytes is not None and self.written_bytes+len(encoded)>self.max_bytes:
            self.error='host total byte admission exceeded'
            raise RuntimeError(self.error)
        self.stream.write(encoded.decode('utf-8'))
        self.stream.flush()
        self.written_bytes+=len(encoded)

    def __enter__(self):
        self.stream = (self.output / "host.jsonl").open("x", encoding="utf-8", newline="\n")
        try:
            self.write({"kind": "start", **observer.stamp(), "interval_seconds": 1,
                        "observer_sha256": sha(OBSERVER), "auxiliary_telemetry_only": True})
            self.sampler = observer.HostSampler()
            # A real sample is persisted before measured work is admitted.
            self.write(self.sampler.sample())
            self.samples += 1
        except BaseException as error:
            self.error=type(error).__name__+': '+str(error)[:256]
            self.finish()
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
            self.error = type(error).__name__+': '+str(error)[:256]

    def finish(self):
        if self.finished is not None:
            return self.finished
        self.stop.set()
        if self.thread:
            self.thread.join(timeout=10)
            if self.thread.is_alive():
                raise RuntimeError("host observation failed to stop; campaign is incomplete")
        try:
            self.write({"kind": "final", **observer.stamp(), "samples": self.samples,
                        "error": self.error, "elapsed_seconds": time.monotonic() - self.started})
        except Exception as error:
            self.error=self.error or type(error).__name__+': '+str(error)[:256]
        try:
            self.stream.flush()
            os.fsync(self.stream.fileno())
        finally:
            self.stream.close()
        self.finished = {"complete": self.error is None and self.samples > 0,
                         "samples": self.samples, "error": self.error,
                         "sha256": sha(self.output / "host.jsonl"),
                         "bytes":self.written_bytes,"max_bytes":self.max_bytes,
                         "max_record_bytes":self.max_record_bytes,
                         "observer_sha256": sha(OBSERVER), "binding_sha256": sha(__file__),
                         "quietness": "not evaluated; unavailable fields never mean idle"}
        with (self.output / "host-receipt.json").open("x", encoding="utf-8", newline="\n") as stream:
            json.dump(self.finished, stream, indent=2, allow_nan=False)
            stream.flush()
            os.fsync(stream.fileno())
        return self.finished

    def __exit__(self, *_args):
        result=self.finish()
        if (self.max_bytes is not None or self.max_record_bytes is not None) and not result['complete']:
            raise RuntimeError('bounded host evidence incomplete; failure retained')
