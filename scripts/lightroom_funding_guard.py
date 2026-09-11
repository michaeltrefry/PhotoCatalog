#!/usr/bin/env python3
"""Supplementary private main-only control; never changes frozen inspection evidence."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import stat
import sys
import time
import types

CAP = 16 * 1024**2


def encoded(value):
    return (json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True) + "\n").encode()


def raw(path):
    path = Path(path)
    if not path.is_absolute() or any(p.is_symlink() for p in [path, *path.parents]):
        raise ValueError("absolute, nonsymlink control evidence required")
    with path.open("rb") as stream:
        meta = os.fstat(stream.fileno())
        if not stat.S_ISREG(meta.st_mode) or meta.st_size > CAP:
            raise ValueError("invalid control evidence size/type")
        result = stream.read(CAP + 1)
    if len(result) > CAP:
        raise ValueError("control evidence grew")
    return result


def verified(reference):
    if not isinstance(reference, dict) or set(reference) != {"path", "sha256"}:
        raise ValueError("unresolved control reference")
    data = raw(reference["path"])
    if hashlib.sha256(data).hexdigest() != reference["sha256"]:
        raise ValueError("control evidence digest mismatch")
    return data


def save_new(path, value):
    with Path(path).open("xb") as stream:
        stream.write(encoded(value))
        stream.flush()
        os.fsync(stream.fileno())
    fd = os.open(Path(path).parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def load_policy(path, digest):
    policy = json.loads(verified({"path": str(path), "sha256": digest}))
    required = {"protocol", "run", "attempt", "adapter", "runner", "helper", "config", "binding",
                "protected_bytes", "operating_reserve_bytes", "command_buffer_bytes",
                "initial_minimum_bytes", "native_core", "native_binary_sha256"}
    if set(policy) != required or policy["protocol"] != 1:
        raise ValueError("invalid funding policy")
    for name in ["protected_bytes", "operating_reserve_bytes", "command_buffer_bytes", "initial_minimum_bytes"]:
        minimum = 0 if name == "protected_bytes" else 1
        if type(policy[name]) is not int or policy[name] < minimum:
            raise ValueError("unresolved funding amount")
    if policy["initial_minimum_bytes"] != sum(policy[n] for n in ["protected_bytes", "operating_reserve_bytes", "command_buffer_bytes"]):
        raise ValueError("funding arithmetic mismatch")
    for name in ["run", "attempt"]:
        value = Path(policy[name])
        if not value.is_absolute() or any(p.is_symlink() for p in [value, *value.parents]):
            raise ValueError("invalid funding control location")
    run = Path(policy["run"])
    if Path(policy["helper"]["path"]) != Path(policy["runner"]["path"]).with_name("lightroom_generation.py"):
        raise ValueError("frozen helper location mismatch")
    for name in ["config", "binding"]:
        if Path(policy[name]["path"]) != run / (name + ".json"):
            raise ValueError("frozen run evidence location mismatch")
    # Verify every code/config input BEFORE executing any frozen source.
    sources = {name: verified(policy[name]) for name in ["adapter", "runner", "helper", "config", "binding"]}
    if Path(policy["adapter"]["path"]) != Path(__file__).resolve():
        raise ValueError("wrong executing adapter")
    config, binding = json.loads(sources["config"]), json.loads(sources["binding"])
    if (config["exclusive_output"] != str(run)
            or binding["driver_sha256"] != policy["runner"]["sha256"]
            or binding["generation_driver_sha256"] != policy["helper"]["sha256"]
            or binding["config_sha256"] != policy["config"]["sha256"]
            or binding["source"] != policy["native_core"]
            or binding["binary_sha256"] != policy["native_binary_sha256"]):
        raise ValueError("frozen source/config/native binding mismatch")
    return policy, sources["runner"]


class FundingMonitor:
    """Samples free bytes; this is a failure/pause guard, not a filesystem quota."""
    def __init__(self, policy, role, free=None):
        if role not in {"adapter", "outer"}:
            raise ValueError("invalid funding observation role")
        self.policy = policy
        self.run = Path(policy["run"])
        self.attempt = Path(policy["attempt"])
        self.role = role
        self.free = free or self.available
        self.initial = None
        self.minimum = None
        self.samples = 0
        self.reason = None
        self.floor = policy["protected_bytes"] + policy["operating_reserve_bytes"]
        self.trigger = self.floor + policy["command_buffer_bytes"]

    def available(self):
        volume = os.statvfs(self.run)
        return volume.f_bavail * volume.f_frsize

    def sample(self):
        value = self.free()
        if type(value) is not int or value < 0:
            raise ValueError("invalid free-space observation")
        self.samples += 1
        if self.initial is None:
            self.initial = value
        changed = self.minimum is None or value < self.minimum
        self.minimum = value if self.minimum is None else min(value, self.minimum)
        if changed:
            # Exclusive append-only record at every newly observed minimum;
            # a crash cannot erase a previously published observation.
            save_new(self.attempt / f"funding-{self.role}-minimum-{self.samples:08d}.json", {
                "initial_free_bytes": self.initial, "minimum_observed_free_bytes": self.minimum,
                "sample": self.samples, "observed_unix": time.time(), "role": self.role,
                "protected_bytes": self.policy["protected_bytes"], "pause_threshold_bytes": self.trigger,
                "emergency_threshold_bytes": self.floor, "measurement": "observed free bytes, not a quota"})
        return value

    def pause(self, reason):
        self.reason = reason
        try:
            save_new(self.run / "pause-request", {"owner": str(self.attempt),
                "reason": reason, "requested_unix": time.time()})
        except FileExistsError:
            # Never replace an existing coordinator/foreign request.
            pass

    def boundary(self, requested, pause_type):
        required = max(requested, self.trigger)
        value = self.sample()
        if value < required:
            self.pause("protected funding: command-boundary free-space admission")
            raise pause_type(self.reason)
        return {"available_bytes": value, "required_bytes": required}

    def outer(self):
        value = self.sample()
        if value < self.trigger:
            self.pause("protected funding: sampled cooperative pause")
        if value <= self.floor:
            self.reason = "protected funding: sampled emergency stop before operating reserve"
            raise RuntimeError(self.reason)

    def finish(self):
        save_new(self.attempt / f"funding-{self.role}-result.json", {
            "initial_free_bytes": self.initial, "minimum_observed_free_bytes": self.minimum,
            "samples": self.samples, "stop_reason": self.reason,
            "protected_bytes": self.policy["protected_bytes"], "operating_reserve_bytes": self.policy["operating_reserve_bytes"],
            "command_buffer_bytes": self.policy["command_buffer_bytes"],
            "measurement": "sampled/command-boundary observation; no hard allocation or quota guarantee"})


def guarded_type(base, monitor, pause_type):
    class FundedRunner(base):
        def space(self, minimum=None):
            return monitor.boundary(self.config["minimum_free_bytes"] if minimum is None else minimum, pause_type)
    return FundedRunner


def execute(policy_path, policy_digest):
    policy, source = load_policy(policy_path, policy_digest)
    monitor = FundingMonitor(policy, "adapter")
    try:
        # Checked source bytes are executed, never a later unverified file reread.
        module = types.ModuleType("funding_bound_frozen_runner")
        module.__file__ = policy["runner"]["path"]
        exec(compile(source, module.__file__, "exec"), module.__dict__)
        module.Runner = guarded_type(module.Runner, monitor, module.PauseRequested)
        old_argv = sys.argv
        try:
            sys.argv = [module.__file__, "main", policy["run"]]
            module.main()
        finally:
            sys.argv = old_argv
    finally:
        monitor.finish()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("policy", type=Path)
    parser.add_argument("policy_sha256")
    args = parser.parse_args()
    execute(args.policy, args.policy_sha256)
