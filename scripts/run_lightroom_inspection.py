#!/usr/bin/env python3
"""Private macOS S9 inspection orchestration; invokes only the frozen inspection CLI.

No migration or choice command exists here. Running a phase requires the coordinator's
resource-lane admission. See the private reviewed run protocol for capture semantics.
"""
import argparse
from collections import Counter
import contextlib
import fcntl
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import signal
import stat
import subprocess
import threading
import time
import uuid
from types import SimpleNamespace

SOURCE = "0f301c14cd6be7784b513db01ed651f527a10213"
# Native gate and exclusive binary preservation are recorded separately from this runner.
BINARY_SHA256 = "3f2b697423f98e9142828c8b4889652c8c20f068ea98097bc4fd70f1fe0971f3"
BINARY_BYTES = 19432736
MIB = 1024 * 1024
PAGES = {"rows", "issues", "paths", "packets", "metadata-conflicts", "path-collisions", "packet-bytes"}
ALLOWED = PAGES | {"discover", "create", "register-inventory", "capture", "add", "resume", "report", "check-paths", "families"}
DEFAULTS = {
    "protocol": 3,
    "seed_request": None,
    "generation_request": None,
    "source_commit": SOURCE,
    "tested_binary_sha256": BINARY_SHA256,
    "closed_application_evidence": None,
    "automatic_choose": False,
    "automatic_migration": False,
    "deadlines_seconds": {"default": 120, "capture": 3600, "resume": 1200, "report": 600, "families": 900, "check-paths": 3600},
    "stdout_caps_bytes": {"page": 8*MIB, "document": 16*MIB, "aggregate": 64*MIB},
    "stderr_cap_bytes": 8*MIB,
    "state_cap_bytes": 16*MIB,
    "minimum_free_bytes": 32*1024*MIB,
    "initial_space_main_multiplier": 12,
    "resume_rows_per_call": 10000,
    "page_limit": 1000,
    "maximum_calls_per_revision": 1000000,
}


def encoded(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode()+b"\n"


def update_canonical_hash(digest, value):
    """Hash the exact encoded() byte stream without a whole-row str/bytes pair."""
    encoder = json.JSONEncoder(sort_keys=True, separators=(",", ":"), ensure_ascii=True)
    for chunk in encoder.iterencode(value):
        # A single escaped string can itself fill a page. Bound its byte copy;
        # tiny numeric chunks can go directly to hashlib without buffering.
        if len(chunk) <= 65536:
            digest.update(chunk.encode())
        else:
            for start in range(0, len(chunk), 65536):
                digest.update(chunk[start:start+65536].encode())
    digest.update(b"\n")


def sha(data):
    return hashlib.sha256(data).hexdigest()


def file_sha(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        while chunk := handle.read(MIB):
            digest.update(chunk)
    return digest.hexdigest()


def read_json(path, cap=16*MIB):
    with open(path, "rb") as handle:
        raw = handle.read(cap+1)
    if len(raw) > cap:
        raise ValueError(f"JSON admission limit exceeded: {path}")
    return json.loads(raw)


def durable_json(path, value, replace=False):
    raw = encoded(value)
    if len(raw) > 16*MIB:
        raise ValueError("journal document exceeds 16 MiB")
    pending = path.with_name(path.name+".pending-"+uuid.uuid4().hex)
    with open(pending, "xb") as handle:
        handle.write(raw)
        handle.flush()
        os.fsync(handle.fileno())
    if replace:
        os.replace(pending, path)
    else:
        os.link(pending, path)
        pending.unlink()
    sync_dir(path.parent)


def sync_dir(path):
    descriptor = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def no_links(path):
    path = Path(os.path.abspath(path))
    for component in [*reversed(path.parents), path]:
        try:
            if stat.S_ISLNK(component.lstat().st_mode):
                raise ValueError(f"symlink component rejected: {component}")
        except FileNotFoundError:
            continue
    return path


def native_path(value):
    if value.get("encoding") != "UnixBytes":
        raise ValueError("private Mac runner requires original UnixBytes paths")
    raw = bytes(value["units"])
    if b"\0" in raw:
        raise ValueError("embedded NUL in path")
    return Path(os.fsdecode(raw))


def candidate_key(candidate):
    return sha(encoded(candidate["path"]))


def revision(stat_result):
    return (stat_result.st_dev, stat_result.st_ino, stat_result.st_size,
            stat_result.st_mtime_ns, stat_result.st_ctime_ns)


def validate_config(config):
    if BINARY_SHA256 is None or BINARY_BYTES is None:
        raise ValueError("corrected core has no tested binary binding yet")
    if config.get("protocol") != 3 or (config.get("seed_request") is not None and not isinstance(config["seed_request"], dict)):
        raise ValueError("invalid protocol or seed request")
    if (config.get("generation_request") is not None and (not isinstance(config["generation_request"], dict) or config.get("seed_request") is not None)):
        raise ValueError("invalid generation request or conflicting seed")
    if (config.get("source_commit") != SOURCE or config.get("tested_binary_sha256") != BINARY_SHA256
            or config.get("closed_application_evidence") is not None
            or config.get("automatic_choose") is not False or config.get("automatic_migration") is not False):
        raise ValueError("unreviewed source binding or prohibited assertion/action")
    for name, default in DEFAULTS.items():
        if name not in config:
            raise ValueError(f"missing frozen configuration field: {name}")
        if isinstance(default, dict):
            if set(config[name]) != set(default) or any(not isinstance(v, int) or v <= 0 for v in config[name].values()):
                raise ValueError(f"invalid bounded configuration: {name}")
    if config["state_cap_bytes"] != 16*MIB:
        raise ValueError("journal document cap is fixed at 16 MiB")
    if config["stdout_caps_bytes"]["page"] != 8*MIB:
        raise ValueError("page framing contract is fixed at 8 MiB")
    if not 1 <= config["page_limit"] <= 1000 or not 1 <= config["resume_rows_per_call"] <= 100000:
        raise ValueError("invalid CLI batch size")
    for field in ["maximum_calls_per_revision", "minimum_free_bytes", "initial_space_main_multiplier", "stderr_cap_bytes", "state_cap_bytes"]:
        if not isinstance(config[field], int) or config[field] <= 0:
            raise ValueError(f"invalid limit: {field}")
    return config


def initialize(config_path):
    config = validate_config(read_json(config_path))
    if config["generation_request"] is not None:
        generation_tools().require_runtime()
    root = no_links(config["exclusive_output"])
    sources = [no_links(config[name]) for name in ["catalog_root", "original_root"]]
    if any(root == source or root in source.parents or source in root.parents for source in sources):
        raise ValueError("output overlaps originals")
    root.mkdir(mode=0o700)  # Exclusive: never reuse another run's root.
    for name in ["commands", "steps", "captures", "reports", "plans"]:
        (root/name).mkdir(mode=0o700)
    durable_json(root/"config.json", config)
    source = no_links(config["tested_binary"])
    descriptor = os.open(source, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode) or before.st_size != BINARY_BYTES:
            raise ValueError("tested binary size/type mismatch")
        copied = hashlib.sha256()
        with os.fdopen(os.dup(descriptor), "rb") as reader, open(root/"lightroom_inspect", "xb") as writer:
            remaining = BINARY_BYTES
            while remaining:
                data = reader.read(min(MIB, remaining))
                if not data:
                    raise ValueError("short binary copy")
                copied.update(data)
                writer.write(data)
                remaining -= len(data)
            if reader.read(1):
                raise ValueError("binary grew during copy")
            writer.flush()
            os.fsync(writer.fileno())
        if (copied.hexdigest() != BINARY_SHA256 or revision(before) != revision(os.fstat(descriptor))
                or revision(before) != revision(source.lstat()) or file_sha(root/"lightroom_inspect") != BINARY_SHA256):
            raise ValueError("tested binary source/copy binding mismatch")
    finally:
        os.close(descriptor)
    os.chmod(root/"lightroom_inspect", 0o500)
    durable_json(root/"binding.json", {"source": SOURCE, "binary_sha256": BINARY_SHA256,
        "binary_bytes": BINARY_BYTES, "driver_sha256": file_sha(__file__), "generation_driver_sha256": file_sha(Path(__file__).with_name("lightroom_generation.py")), "config_sha256": sha(encoded(config)),
        "created_unix": time.time(), "application_consistency": "unverified"})
    durable_json(root/"journal.json", {"next_command": 1})
    sync_dir(root)
    return root


class InterruptedOperation(RuntimeError):
    """An unclosed journal operation needs review, not a guessed retry."""


class PauseRequested(InterruptedOperation):
    """Coordinator requests yielding before another subprocess, never mid-capture."""


def capture_summary(value):
    if not isinstance(value, dict):
        return None
    fields = ["state", "raw_byte_retention", "sqlite_consistency", "application_consistency", "logical_blake3", "revision_id"]
    return {**{name: value.get(name) for name in fields},
            "artifact_count": len(value.get("artifacts", [])),
            "sqlite_artifacts": [{"role": a["role"], "blake3": a["blake3"], "revision": a["revision"]} for a in value.get("artifacts", []) if a["role"] in {"main", "wal", "shm", "journal"}]}


def unchanged_inventory(delta):
    return delta.get("before_complete") is True and delta.get("after_complete") is True and all(delta.get(field) == [] for field in ["added", "removed", "changed"])


def inventory_delta(before, after):
    old = {candidate_key(v): v for v in before["candidates"]}
    new = {candidate_key(v): v for v in after["candidates"]}
    return {"added": sorted(new.keys()-old.keys()), "removed": sorted(old.keys()-new.keys()),
            "changed": sorted(k for k in old.keys() & new.keys() if old[k] != new[k]),
            "before_complete": before["complete"], "after_complete": after["complete"]}


def private_child(root, relative):
    path = Path(relative)
    if path.is_absolute() or not path.parts or any(part in {".", ".."} for part in path.parts):
        raise ValueError("unsafe private evidence path")
    return no_links(root/path)


def evidence_document(root, reference):
    path = private_child(root, reference["path"])
    # One bounded read binds the parsed document to the reviewed request.
    with open(path, "rb") as handle:
        raw = handle.read(16*MIB+1)
    if len(raw) > 16*MIB or sha(raw) != reference["sha256"]:
        raise ValueError("predecessor evidence digest/size mismatch")
    return json.loads(raw)


def native_revision(value):
    return {"object": f"{value.st_dev}:{value.st_ino}", "bytes": value.st_size,
            "modified_ns": value.st_mtime_ns,
            "changed": f"{value.st_ctime_ns//1000000000}:{value.st_ctime_ns%1000000000}"}


def copy_seed_snapshot(source, destination, expected):
    """Exclusive bounded copy after native capture/add verified the BLAKE3 evidence."""
    source = no_links(source)
    descriptor = os.open(source, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode) or native_revision(before) != expected:
            raise ValueError("private logical snapshot revision mismatch")
        digest = hashlib.sha256()
        with os.fdopen(os.dup(descriptor), "rb") as reader, open(destination, "xb") as writer:
            remaining = before.st_size
            while remaining:
                chunk = reader.read(min(remaining, MIB))
                if not chunk:
                    raise ValueError("short seed snapshot")
                writer.write(chunk)
                digest.update(chunk)
                remaining -= len(chunk)
            if reader.read(1):
                raise ValueError("seed snapshot grew")
            writer.flush()
            os.fsync(writer.fileno())
        if revision(before) != revision(os.fstat(descriptor)) or revision(before) != revision(source.lstat()):
            raise ValueError("seed snapshot changed during copy")
        if file_sha(destination) != digest.hexdigest():
            raise ValueError("seed copy digest mismatch")
        sync_dir(destination.parent)
        return {"source": str(source), "source_revision": expected,
                "bytes": before.st_size, "sha256": digest.hexdigest()}
    finally:
        os.close(descriptor)


def seed_table_counts(report, request):
    tables = report["tables"]
    counts = {table["name"]: table["retained"] for table in tables}
    if (len(counts) != len(tables) or counts != request["expected_table_counts"]
            or sum(counts.values()) != request["expected_rows"]
            or any(table["state"] != "complete" or table["expected"] != table["retained"] for table in tables)
            or report["revision_id"] != request["revision"]):
        raise ValueError("seed retained table counts/identity/state disagree with reviewed evidence")
    return counts


def seed_rows_equal(before, after):
    fields = ["rows", "counts", "last_sequence", "content_sha256", "identity_sha256"]
    if any(before[field] != after[field] for field in fields):
        raise ValueError("corrective reconciliation changed retained rows or source IDs")


def generation_tools():
    spec = importlib.util.spec_from_file_location("lightroom_generation", Path(__file__).with_name("lightroom_generation.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module.RUN = SimpleNamespace(**globals())
    return module


class Runner:
    def __init__(self, root):
        self.root = no_links(root)
        self.config = validate_config(read_json(self.root/"config.json"))
        self.binding = read_json(self.root/"binding.json")
        self.binary = self.root/"lightroom_inspect"
        if (self.binding["driver_sha256"] != file_sha(__file__)
                or self.binding.get("generation_driver_sha256") != file_sha(Path(__file__).with_name("lightroom_generation.py"))
                or self.binding["config_sha256"] != sha(encoded(self.config))
                or self.binding["source"] != SOURCE or file_sha(self.binary) != BINARY_SHA256):
            raise ValueError("run source/config/binary changed; explicit new run required")
        self.binary_revision = revision(self.binary.lstat())
        self.lock = open(self.root/"runner.lock", "a+b")
        fcntl.flock(self.lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        self.generation = None
        try:
            if self.config["generation_request"] is not None and (self.root/"reports/generation-adoption.json").exists():
                tools = generation_tools()
                protocol = self.config["generation_request"].get("protocol")
                replay_type = {1: tools.Replay, 2: tools.RepairReplay, 3: tools.PidRepairReplay}.get(protocol)
                if replay_type is None:
                    raise ValueError("unsupported generation replay protocol")
                self.generation = replay_type(self)
        except BaseException:
            self.lock.close()
            raise

    def close(self):
        if self.generation is not None:
            self.generation.close()
        self.lock.close()

    def space(self, minimum=None):
        available = os.statvfs(self.root).f_bavail * os.statvfs(self.root).f_frsize
        needed = self.config["minimum_free_bytes"] if minimum is None else minimum
        if available < needed:
            raise RuntimeError(f"private space admission failed: available={available}, required={needed}")
        return {"available_bytes": available, "required_bytes": needed}

    def call(self, key, arguments):
        """Replay a completed journal entry, never automatically retry a failed command."""
        command = arguments[0]
        if command not in ALLOWED or "--closed-application-evidence" in arguments:
            raise ValueError("command outside inspection-only scope")
        step_path = self.root/"steps"/(sha(encoded(key))+".json")
        if step_path.exists():
            step = read_json(step_path)
            record_path = self.root/step["record"]
            if not record_path.exists():
                raise InterruptedOperation(f"interrupted command requires owned-process/publication review: {step}")
            record = read_json(record_path)
            if record["requested_arguments"] != [str(v) for v in arguments]:
                raise ValueError("replayed operation arguments differ")
            return self.result(record)
        if self.generation is not None:
            adopted = self.generation.replay(key, arguments)
            if adopted is not None:
                return adopted
        if os.path.lexists(self.root/"pause-request"):
            # Checked before reserving a command: a pause has no failed/orphan child.
            raise PauseRequested("coordinator pause requested at command boundary; remove owned pause-request to resume")
        self.space()
        if revision(self.binary.lstat()) != self.binary_revision:
            raise ValueError("bound binary changed during phase")
        journal = read_json(self.root/"journal.json")
        sequence = journal["next_command"]
        journal["next_command"] += 1
        durable_json(self.root/"journal.json", journal, replace=True)
        directory = self.root/"commands"/f"{sequence:09d}"
        directory.mkdir(mode=0o700)
        capture_path = self.root/"captures"/f"{sequence:09d}"
        argv = [str(self.binary), *[str(v) if str(v) != "@CAPTURE@" else str(capture_path) for v in arguments]]
        record = {"sequence": sequence, "key": key, "requested_arguments": [str(v) for v in arguments],
                  "argv": argv, "capture_path": str(capture_path) if command == "capture" else None,
                  "started_unix": time.time(), "source_binding": self.binding}
        durable_json(directory/"started.json", record)
        durable_json(step_path, {"record": str((directory/"result.json").relative_to(self.root)), "sequence": sequence})
        caps = self.config["stdout_caps_bytes"]
        cap = caps["page" if command in PAGES else "aggregate" if command in {"report", "families"} else "document"]
        deadline = self.config["deadlines_seconds"].get(command, self.config["deadlines_seconds"]["default"])
        record.update(stdout_cap=cap, stderr_cap=self.config["stderr_cap_bytes"], deadline_seconds=deadline)
        excess = threading.Event()
        errors = []
        process = None
        threads = []
        failure = None
        def drain(stream, path, maximum):
            try:
                count = 0
                with open(path, "xb") as output:
                    while chunk := stream.read(65536):
                        keep = chunk[:max(0, maximum-count)]
                        output.write(keep)
                        count += len(chunk)
                        if count > maximum:
                            excess.set()
                    output.flush()
                    os.fsync(output.fileno())
            except BaseException as error:
                errors.append(repr(error))
                excess.set()
            finally:
                stream.close()
        try:
            process = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                       stderr=subprocess.PIPE, start_new_session=True)
            durable_json(directory/"process.json", {"pid": process.pid, "process_group": process.pid, "started_unix": time.time(), "argv": argv})
            for stream, name, maximum in [(process.stdout, "stdout", cap), (process.stderr, "stderr", record["stderr_cap"])]:
                thread = threading.Thread(target=drain, args=(stream, directory/name, maximum))
                thread.start()
                threads.append(thread)
            end = time.monotonic()+deadline
            while process.poll() is None:
                if excess.is_set() or time.monotonic() >= end:
                    failure = "output_limit" if excess.is_set() else "deadline"
                    break
                time.sleep(0.05)
        except BaseException as error:
            failure = f"interrupted_or_launch_error: {error!r}"
        finally:
            if process is not None:
                # Kill the owned group on failure, including a nested capture-worker.
                if failure:
                    with contextlib.suppress(ProcessLookupError):
                        os.killpg(process.pid, signal.SIGKILL)
                process.wait()
            for thread in threads:
                thread.join(timeout=5)
            if any(thread.is_alive() for thread in threads):
                failure = failure or "pipe_drain_deadline"
                if process is not None:
                    with contextlib.suppress(ProcessLookupError):
                        os.killpg(process.pid, signal.SIGKILL)
                for thread in threads:
                    thread.join(timeout=5)
                if any(thread.is_alive() for thread in threads):
                    # The started record remains explicit; never fabricate completed logs.
                    raise InterruptedOperation("owned pipe/log drain failed to finish")
            if excess.is_set() and failure is None:
                failure = "output_limit_or_log_error"
            record.update(exit_code=None if process is None else process.returncode,
                          failure=failure, log_errors=errors, finished_unix=time.time())
            for name in ["stdout", "stderr"]:
                path = directory/name
                record[name] = {"path": str(path.relative_to(self.root)), "sha256": file_sha(path), "bytes": path.stat().st_size} if path.exists() else None
            durable_json(directory/"result.json", record)
        if failure and failure.startswith("interrupted_or_launch_error"):
            raise InterruptedOperation(failure)
        return self.result(record)

    def result(self, record):
        value = None
        artifact = record["stdout"]
        if artifact:
            path = self.root/artifact["path"]
            if file_sha(path) != artifact["sha256"]:
                raise ValueError("journal stdout digest changed")
            try:
                value = read_json(path, record["stdout_cap"])
            except (ValueError, UnicodeError):
                pass
        ok = record["exit_code"] == 0 and not record["failure"] and not record["log_errors"] and value is not None
        return {"ok": ok, "value": value, "record": record}

    def command_document(self, sequence, command):
        if self.generation is not None and isinstance(sequence, str) and sequence.startswith("adopted/"):
            return self.generation.document(sequence, command)
        if not isinstance(sequence, int) or sequence < 1:
            raise ValueError("invalid command provenance")
        record = read_json(self.root/"commands"/f"{sequence:09d}"/"result.json")
        if record["requested_arguments"][0] != command:
            raise ValueError("unexpected provenance command")
        result = self.result(record)
        if not result["ok"]:
            raise ValueError("failed command cannot supply admission evidence")
        return result["value"]

    def require(self, key, arguments):
        result = self.call(key, arguments)
        if not result["ok"]:
            raise RuntimeError(f"required operation failed; retained command {result['record']['sequence']}")
        return result

    def pages(self, plan, revision_id, name, key):
        after = 0
        counts = {}
        page_records = []
        total = 0
        available_bytes = 0
        content_digest = hashlib.sha256()
        identity_digest = hashlib.sha256()
        for page_number in range(self.config["maximum_calls_per_revision"]):
            page = self._consume_page(plan, revision_id, name, key, page_number,
                                      after, counts, content_digest, identity_digest)
            if page is None:
                return {"rows": total, "counts": counts, "last_sequence": after, "pages": page_records, "available_reference_bytes": available_bytes, "content_sha256": content_digest.hexdigest(), "identity_sha256": identity_digest.hexdigest()}
            after, rows, size, record_sequence = page
            total += rows
            available_bytes += size
            page_records.append(record_sequence)
        raise RuntimeError("page-call admission exhausted; enumeration remains incomplete")

    def _consume_page(self, plan, revision_id, name, key, page_number, after,
                      counts, content_digest, identity_digest):
        # This frame owns every reference to the decoded page, including the last
        # row and path metadata. Return only scalars before loading another page.
        result = self.require([key, revision_id, name, page_number], [name, plan, revision_id, "--after", after, "--limit", self.config["page_limit"]])
        values = result["value"]
        if not isinstance(values, list):
            raise ValueError("page is not an array")
        if not values:
            return None
        available_bytes = 0
        for value in values:
            sequence = value["sequence"]
            if not isinstance(sequence, int) or sequence <= after:
                raise ValueError("non-increasing page cursor")
            after = sequence
            if name == "rows" and (value.get("revision_id") != revision_id or not value.get("source_id")):
                raise ValueError("row source/revision identity mismatch")
            if name == "paths":
                metadata = (value.get("evidence") or {}).get("metadata", {})
                if value.get("state", "").startswith("available"):
                    available_bytes += metadata.get("bytes", 0)
            update_canonical_hash(content_digest, value)
            update_canonical_hash(identity_digest, [sequence, value.get("source_id"), value.get("revision_id"), value.get("table")])
            label = value.get("table", value.get("state", value.get("origin", value.get("code", "record"))))
            counts[label] = counts.get(label, 0)+1
        return after, len(values), available_bytes, result["record"]["sequence"]

    def inspect(self, plan, capture_path, key):
        added = self.require([key, "add"], ["add", plan, capture_path])
        revision_id = added["value"]["revision"]
        for index in range(self.config["maximum_calls_per_revision"]):
            value = self.require([key, "resume", index], ["resume", plan, revision_id, "--max-rows", self.config["resume_rows_per_call"]])["value"]
            if value["stage"] != "pending":
                break
            if value["retained_this_call"] == 0:
                raise RuntimeError("pending inspection made no progress")
        else:
            raise RuntimeError("resume-call admission exhausted")
        report = self.require([key, "report"], ["report", plan, revision_id])
        pages = {name: self.pages(plan, revision_id, name, key) for name in ["rows", "issues", "packets", "metadata-conflicts"]}
        counts = pages["rows"]["counts"]
        mismatches = [table for table in report["value"]["tables"] if counts.get(table["name"], 0) != table["retained"]]
        if mismatches:
            raise RuntimeError("paged retained totals disagree with source-table report")
        return {"revision": revision_id, "capture_path": str(capture_path), "report": report["record"]["sequence"],
                "stage": report["value"]["stage"], "table_states": report["value"]["tables"], "counts": report["value"]["counts"], "pages": pages}

    def summary(self, name, value):
        path = self.root/"reports"/(name+".json")
        # Completed stage reports are immutable. Restart must reproduce their exact data.
        if path.exists():
            if read_json(path) != value:
                raise ValueError("stage report changed; new evidence generation required")
        else:
            durable_json(path, value)
        return path

    def seed_phase(self):
        request = self.config["seed_request"]
        if not request or request.get("protocol") != 1:
            raise ValueError("a reviewed seed request is required")
        predecessor = no_links(request["predecessor"])
        # This corrective path only admits an owned sibling private run, never an
        # arbitrary database under either original root.
        if predecessor.parent != self.root.parent or predecessor == self.root:
            raise ValueError("seed predecessor must be a separate private sibling run")
        for field in ["catalog_root", "original_root"]:
            original = no_links(self.config[field])
            if predecessor == original or original in predecessor.parents or predecessor in original.parents:
                raise ValueError("seed predecessor overlaps originals")
        descriptor = os.open(no_links(predecessor/"runner.lock"), os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        try:
            if not stat.S_ISREG(os.fstat(descriptor).st_mode):
                raise ValueError("predecessor lock is not regular")
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
            if not os.path.lexists(predecessor/"pause-request"):
                raise ValueError("predecessor is not held paused")
            return self.adopt_seed(predecessor, request)
        finally:
            os.close(descriptor)

    def adopt_seed(self, predecessor, request):
        binding = evidence_document(predecessor, request["binding"])
        paused = evidence_document(predecessor, request["paused_phase"])
        failed = evidence_document(predecessor, request["failed_result"])
        stop = evidence_document(predecessor, request["stop_decision"])
        table_evidence = evidence_document(predecessor, request["table_state_evidence"])
        if (request["binding"]["path"] != "binding.json"
                or request["failed_result"]["path"] != f"commands/{failed['sequence']:09d}/result.json"
                or paused.get("status") != "paused" or paused.get("binding") != binding
                or failed.get("source_binding") != binding or failed.get("exit_code", 0) >= 0
                or failed.get("requested_arguments") != ["resume", str(predecessor/"plans/main"), request["revision"], "--max-rows", "10000"]
                or read_json(predecessor/"journal.json")["next_command"] != failed["sequence"]+1):
            raise ValueError("predecessor is not the reviewed stopped pending reconciliation")

        def old_command(reference, command):
            record = evidence_document(predecessor, reference)
            if (reference["path"] != f"commands/{record['sequence']:09d}/result.json"
                    or record.get("source_binding") != binding or record.get("exit_code") != 0
                    or record.get("failure") or record.get("log_errors")
                    or record["requested_arguments"][0] != command):
                raise ValueError("invalid predecessor command provenance")
            value = evidence_document(predecessor, record["stdout"])
            return record, value

        inventory_record, inventory = old_command(request["inventory_result"], "discover")
        capture_record, capture = old_command(request["capture_result"], "capture")
        candidates = [v for v in inventory["candidates"] if candidate_key(v) == request["candidate_key"]]
        capture_path = private_child(predecessor, request["capture_path"])
        if (not inventory["complete"] or len(candidates) != 1
                or inventory_record["requested_arguments"] != ["discover", self.config["catalog_root"]]
                or capture_record["requested_arguments"] != ["capture", str(native_path(candidates[0]["path"])), "@CAPTURE@", "--main-only"]
                or capture_record["capture_path"] != str(capture_path)
                or capture.get("state") != "captured" or capture.get("revision_id") != request["revision"]
                or capture.get("sqlite_consistency") != "consistent_default_sqlite"):
            raise ValueError("seed raw capture/inventory identity mismatch")
        key = ["seed", sha(encoded(request))]
        plan = self.root/"plans/main"
        # A real capture command preserves the old private main/WAL/SHM bytes.
        # No SQLite connection is ever opened against the old plan by Python.
        preserved = self.require([key, "capture-private-plan"], ["capture", predecessor/"plans/main/inspection.sqlite3", "@CAPTURE@"])
        manifest = preserved["value"]
        if (manifest.get("state") != "captured" or manifest.get("raw_byte_retention") != "complete"
                or manifest.get("sqlite_consistency") != "consistent_default_sqlite"):
            raise ValueError("private plan capture is unsafe/incomplete")
        private_capture = Path(preserved["record"]["capture_path"])
        verifier = self.root/"plans/seed-capture-verification"
        self.require([key, "create-verifier"], ["create", verifier])
        self.require([key, "verify-private-capture"], ["add", verifier, private_capture])
        copy_receipt = self.root/"reports/seed-copy.json"
        if not copy_receipt.exists():
            if os.path.lexists(self.root/"reports/seed-copy-started.json") or plan.exists():
                raise InterruptedOperation("unpublished seed copy requires review; never overwrite it")
            if os.path.lexists(self.root/"pause-request"):
                raise PauseRequested("coordinator pause before private snapshot copy")
            self.space(2*manifest["logical_revision"]["bytes"]+self.config["minimum_free_bytes"])
            self.summary("seed-copy-started", {"request_sha256": key[1], "capture_command": preserved["record"]["sequence"]})
            plan.mkdir(mode=0o700)
            sync_dir(plan.parent)
            copied = copy_seed_snapshot(private_capture/"logical.sqlite3", plan/"inspection.sqlite3", manifest["logical_revision"])
            self.summary("seed-copy", {**copied, "request_sha256": key[1], "capture_command": preserved["record"]["sequence"]})
        elif read_json(copy_receipt)["request_sha256"] != key[1]:
            raise ValueError("seed copy belongs to different evidence")

        added = self.require([key, "verify-original-capture"], ["add", plan, capture_path])
        if added["value"]["revision"] != request["revision"]:
            raise ValueError("seed source revision changed")
        before = self.require([key, "before-report"], ["report", plan, request["revision"]])["value"]
        counts = seed_table_counts(before, request)
        if before["stage"] != "pending" or before["capture"] != capture:
            raise ValueError("seed is not the preserved pending source capture")
        families = self.require([key, "before-families"], ["families", plan])["value"]["families"]
        if (any(family["selected"] is not None for family in families)
                or [member["revision_id"] for family in families for member in family["members"]] != [request["revision"]]):
            raise ValueError("seed contains unexpected revisions or carried choices")
        rows = self.pages(plan, request["revision"], "rows", [key, "before"])
        if rows["counts"] != {name: count for name, count in counts.items() if count} or rows["rows"] != request["expected_rows"]:
            raise ValueError("seed pages disagree with retained row counts")
        # All tables are already retained. This actual corrected resume must only
        # finish reconciliation; failed commands remain terminal journal evidence.
        progress = self.require([key, "corrective-reconcile"], ["resume", plan, request["revision"], "--max-rows", self.config["resume_rows_per_call"]])["value"]
        if progress["retained_this_call"] != 0 or progress["stage"] not in {"rows_reconciled_paths_pending", "inspection_complete_with_reported_gaps"}:
            raise ValueError("corrective reconciliation did not finish the retained plan")
        inspection = self.inspect(plan, capture_path, [key, "after"])
        after = self.command_document(inspection["report"], "report")
        seed_table_counts(after, request)
        seed_rows_equal(rows, inspection["pages"]["rows"])
        if before["lineage_id"] != after["lineage_id"] or before["capture"] != after["capture"]:
            raise ValueError("corrective reconciliation changed source namespace/evidence")
        return self.summary("seed-adoption", {"protocol": 1, "request_sha256": key[1],
            "predecessor": str(predecessor), "predecessor_binding": binding,
            "failed_command": failed, "stop_decision": stop, "table_state_evidence": table_evidence,
            "private_plan_capture_command": preserved["record"]["sequence"], "copy_receipt_sha256": file_sha(copy_receipt),
            "original_inventory_command": inventory_record["sequence"], "original_capture_command": capture_record["sequence"],
            "original_inventory": inventory,
            "candidate": candidates[0], "candidate_key": request["candidate_key"], "capture_path": str(capture_path),
            "capture": capture_summary(capture), "lineage_id": before["lineage_id"], "before_rows": rows,
            "inspection": inspection, "automatic_selection": False, "migration_executed": False,
            "application_consistency": "unverified", "source_binding": self.binding})

    def main_phase(self):
        plan = self.root/"plans"/"main"
        seed = None
        if self.config["generation_request"] is not None and self.generation is None:
            raise ValueError("main requires completed generation adoption")
        if self.config["seed_request"] is not None:
            seed = read_json(self.root/"reports/seed-adoption.json")
            if seed["request_sha256"] != sha(encoded(self.config["seed_request"])) or seed["source_binding"] != self.binding:
                raise ValueError("main seed adoption binding mismatch")
        inventory = generation_tools().admit_inventory(self) if self.generation is not None else self.require(["main", "discover"], ["discover", self.config["catalog_root"]])
        value = inventory["value"]
        if seed:
            delta = inventory_delta(seed["original_inventory"], value)
            self.summary("seed-inventory-admission", {"adoption_sha256": file_sha(self.root/"reports/seed-adoption.json"),
                         "inventory_command": inventory["record"]["sequence"], "delta": delta,
                         "admitted": unchanged_inventory(delta)})
            if not unchanged_inventory(delta):
                raise ValueError("inventory changed since preserved seed; new evidence required")
        estimated = sum(candidate["bytes"] for candidate in value["candidates"]) * self.config["initial_space_main_multiplier"] + self.config["minimum_free_bytes"]
        if not (self.root/"reports"/"initial-space.json").exists():
            self.summary("initial-space", self.space(estimated))
        else:
            self.space()
        if seed is None and self.generation is None:
            self.require(["main", "create"], ["create", plan])
        inventory_path = self.root/inventory["record"]["stdout"]["path"]
        if self.generation is None:
            self.require(["main", "register"], ["register-inventory", plan, inventory_path])
        outcomes = []
        for candidate in sorted(value["candidates"], key=lambda c: bytes(c["path"]["units"])):
            key = candidate_key(candidate)
            outcome = {"candidate": candidate, "key": key, "status": "incomplete"}
            try:
                if self.generation is not None:
                    adopted = self.generation.outcome(key, candidate)
                    if adopted is not None:
                        outcomes.append(adopted)
                        self.summary("main-"+key, adopted)
                        continue
                if seed and key == seed["candidate_key"]:
                    outcome.update(capture_path=seed["capture_path"], capture=seed["capture"],
                                   seed_adoption_sha256=file_sha(self.root/"reports/seed-adoption.json"),
                                   inspection=seed["inspection"], status="inspected_with_reported_limits")
                    outcomes.append(outcome)
                    self.summary("main-"+key, outcome)
                    continue
                capture = self.call(["main", key, "capture"], ["capture", native_path(candidate["path"]), "@CAPTURE@", "--main-only"])
                outcome.update(capture_command=capture["record"]["sequence"], capture_path=capture["record"]["capture_path"], capture=capture_summary(capture["value"]))
                if capture["ok"] and capture["value"].get("state") == "captured":
                    outcome["inspection"] = self.inspect(plan, outcome["capture_path"], ["main", key])
                    outcome["status"] = "inspected_with_reported_limits"
                else:
                    outcome["status"] = "capture_failed_or_unsafe"
            except InterruptedOperation:
                raise
            except Exception as error:
                outcome["error"] = repr(error)
            outcomes.append(outcome)
            self.summary("main-"+key, outcome)
        ending = self.require(["main", "discover-end"], ["discover", self.config["catalog_root"]])
        families = self.require(["main", "families"], ["families", plan])
        review = {"inventory_command": inventory["record"]["sequence"], "ending_inventory_command": ending["record"]["sequence"], "inventory_delta": inventory_delta(value, ending["value"]), "inventory_complete": value["complete"],
                  "candidate_count": len(value["candidates"]), "outcome_counts": dict(Counter(item["status"] for item in outcomes)), "application_unknown_count": sum((item.get("capture") or {}).get("application_consistency") != "closed_application_asserted" for item in outcomes), "outcomes": outcomes, "families": families["value"],
                  "automatic_selection": False, "application_consistency": "unverified", "migration_executed": False}
        review_path = self.summary("main-review", review)
        by_revision = {item["inspection"]["revision"]: item["key"] for item in outcomes if "inspection" in item}
        requests = []
        ambiguous = []
        for family in families["value"]["families"]:
            if family["suggested"]:
                requests.append({"candidate_key": by_revision[family["suggested"]], "revision": family["suggested"],
                                 "family_evidence_digest": family["evidence_digest"], "role": "prospective_suggestion",
                                 "reason": "Prospective full preservation for review; remains unselected"})
            else:
                ambiguous.append({"family": family["id"], "evidence_digest": family["evidence_digest"],
                                  "members": [by_revision[member["revision_id"]] for member in family["members"]], "issues": family["issues"]})
        self.summary("full-capture-request-template", {"main_review_sha256": file_sha(review_path),
                     "automatic_selection": False, "requests": requests, "ambiguous_unselected": ambiguous})
        return review_path

    def full_phase(self, request_path):
        review_path = self.root/"reports"/"main-review.json"
        review = read_json(review_path)
        request = read_json(request_path)
        if request["main_review_sha256"] != file_sha(review_path) or request.get("automatic_selection") is not False:
            raise ValueError("full-copy request is not bound to the current review")
        if not review["inventory_complete"] or any(review["inventory_delta"][field] for field in ["added", "removed", "changed"]) or not review["inventory_delta"]["after_complete"]:
            raise ValueError("main discovery is incomplete or changed; reconcile before full stage")
        request_id = sha(encoded(request))
        requested = {item["candidate_key"]: item for item in request["requests"]}
        if not requested or len(requested) != len(request["requests"]):
            raise ValueError("duplicate full-capture request")
        members = {member["revision_id"]: family for family in review["families"]["families"] for member in family["members"]}
        outcomes = {item["key"]: item for item in review["outcomes"]}
        for key, item in requested.items():
            revision_id = outcomes[key]["inspection"]["revision"]
            if item["revision"] != revision_id or item["family_evidence_digest"] != members[revision_id]["evidence_digest"] or not item["reason"].strip():
                raise ValueError("full-copy request candidate/evidence mismatch")
            if item["role"] not in {"prospective_suggestion", "additional_ambiguity_evidence"}:
                raise ValueError("unknown full-copy role")
            if item["role"] == "prospective_suggestion" and members[revision_id]["suggested"] != revision_id:
                raise ValueError("requested prospective member was not suggested")
        fresh = self.require(["full", request_id, "discover"], ["discover", self.config["catalog_root"]])
        initial = {item["key"]: item["candidate"] for item in review["outcomes"]}
        current = {candidate_key(item): item for item in fresh["value"]["candidates"]}
        if initial != current or not fresh["value"]["complete"]:
            raise RuntimeError("current discovery changed/incomplete; new main evidence generation required")
        replacements = {}
        for key in sorted(requested):
            result = self.call(["full", request_id, key, "capture"], ["capture", native_path(outcomes[key]["candidate"]["path"]), "@CAPTURE@"])
            replacements[key] = {"command": result["record"]["sequence"], "capture_path": result["record"]["capture_path"], "capture": capture_summary(result["value"]), "ok": result["ok"] and result["value"].get("state") == "captured"}
        plan = self.root/"plans"/("final-"+request_id)
        self.require(["full", request_id, "create"], ["create", plan])
        self.require(["full", request_id, "register"], ["register-inventory", plan, self.root/fresh["record"]["stdout"]["path"]])
        final = []
        for key, prior in outcomes.items():
            item = {"key": key, "source": prior["candidate"]["path"], "full_requested": key in requested, "full": replacements.get(key)}
            chosen = replacements.get(key)
            if not chosen or not chosen["ok"]:
                chosen = prior
            try:
                if (chosen.get("capture") or {}).get("state") != "captured":
                    raise RuntimeError("no consistent capture available")
                item["inspection"] = self.inspect(plan, chosen["capture_path"], ["full", request_id, key])
            except InterruptedOperation:
                raise
            except Exception as error:
                item["error"] = repr(error)
            final.append(item)
        ending = self.require(["full", request_id, "discover-end"], ["discover", self.config["catalog_root"]])
        families = self.require(["full", request_id, "families"], ["families", plan])
        return self.summary("full-review-"+request_id, {"request": request, "request_id": request_id, "starting_inventory_command": fresh["record"]["sequence"], "ending_inventory_command": ending["record"]["sequence"], "inventory_delta": inventory_delta(fresh["value"], ending["value"]), "plan": str(plan), "outcome_counts": {"candidates": len(final), "full_requested": len(requested), "full_capture_failures": sum(not item["ok"] for item in replacements.values()), "inspection_failures": sum("error" in item for item in final), "main_only_members": sum(not item.get("full") or not item["full"]["ok"] for item in final)}, "outcomes": final, "families": families["value"], "automatic_selection": False, "application_consistency": "unverified", "migration_executed": False})

    def path_phase(self, full_review_path, packets):
        review = read_json(full_review_path)
        key = ["packets" if packets else "paths", file_sha(full_review_path)]
        if not unchanged_inventory(review.get("inventory_delta", {})):
            raise ValueError("full-review ending inventory changed or incomplete; direct file I/O is not admitted")
        for candidate in review["outcomes"]:
            if candidate["full_requested"]:
                full = candidate.get("full") or {}
                inspection = candidate.get("inspection") or {}
                capture = full.get("capture") or {}
                if (full.get("ok") is not True or capture.get("state") != "captured"
                        or capture.get("raw_byte_retention") != "complete"
                        or capture.get("sqlite_consistency") != "consistent_default_sqlite"
                        or inspection.get("capture_path") != full.get("capture_path")
                        or not inspection.get("revision")):
                    raise ValueError("requested full capture is failed/incomplete or fell back to main-only; direct file I/O is not admitted")
        if packets:
            metadata_review = read_json(self.root/"reports"/("paths-review-"+key[1]+".json"))
            if (metadata_review.get("full_review_sha256") != key[1]
                    or not unchanged_inventory(metadata_review.get("inventory_delta", {}))
                    or any("error" in item for item in metadata_review["outcomes"])):
                raise ValueError("metadata-only path assessment is incomplete/changed; inspect its failures before packet I/O")
        baseline = self.command_document(review["ending_inventory_command"], "discover")
        invocation = uuid.uuid4().hex
        fresh = self.require([key, invocation, "discover-admission"], ["discover", self.config["catalog_root"]])
        delta = inventory_delta(baseline, fresh["value"])
        self.summary(key[0]+"-admission-"+invocation, {"full_review_sha256": key[1],
            "baseline_inventory_command": review["ending_inventory_command"], "fresh_inventory_command": fresh["record"]["sequence"],
            "inventory_delta": delta, "admitted": unchanged_inventory(delta)})
        if not unchanged_inventory(delta):
            raise ValueError("fresh inventory differs from full review; no direct file lookup performed")
        completed = self.root/"reports"/(key[0]+"-review-"+key[1]+".json")
        if completed.exists():
            prior = read_json(completed)
            if not unchanged_inventory(prior.get("inventory_delta", {})):
                raise ValueError("prior path/packet phase observed changed inventory; new evidence generation required")
            return completed
        outcomes = []
        for candidate in review["outcomes"]:
            if not candidate["full_requested"] or "inspection" not in candidate:
                continue
            revision_id = candidate["inspection"]["revision"]
            item = {"key": candidate["key"], "revision": revision_id}
            try:
                for index in range(self.config["maximum_calls_per_revision"]):
                    args = ["check-paths", review["plan"], revision_id, "--limit", self.config["page_limit"]]
                    if packets:
                        args.append("--packets")
                    result = self.require([key, revision_id, "check", index], args)
                    if result["value"]["checked"] == 0:
                        break
                else:
                    raise RuntimeError("path-call admission exhausted")
                names = ["paths", "packets", "metadata-conflicts", "issues"]
                item["pages"] = {name: self.pages(review["plan"], revision_id, name, key) for name in names}
                item["report"] = self.require([key, revision_id, "report"], ["report", review["plan"], revision_id])["value"]
            except InterruptedOperation:
                raise
            except Exception as error:
                item["error"] = repr(error)
            outcomes.append(item)
        ending = self.require([key, invocation, "discover-end"], ["discover", self.config["catalog_root"]])
        families = self.require([key, "families"], ["families", review["plan"]])["value"]
        return self.summary(key[0]+"-review-"+key[1], {"full_review_sha256": key[1], "starting_inventory_command": fresh["record"]["sequence"], "ending_inventory_command": ending["record"]["sequence"], "inventory_delta": inventory_delta(fresh["value"], ending["value"]), "outcome_counts": {"requested_members": len(outcomes), "failures": sum("error" in item for item in outcomes)}, "outcomes": outcomes, "families": families, "automatic_selection": False, "application_consistency": "unverified", "migration_executed": False})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="phase", required=True)
    commands.add_parser("init").add_argument("config", type=Path)
    for phase in ["seed", "adopt", "main", "full", "paths", "packets"]:
        command = commands.add_parser(phase)
        command.add_argument("run", type=Path)
        if phase not in {"main", "seed", "adopt"}:
            command.add_argument("input", type=Path)
    args = parser.parse_args()
    if args.phase == "init":
        print(initialize(args.config))
        return
    runner = Runner(args.run)
    attempt = {"phase": args.phase, "input": str(getattr(args, "input", "")), "started_unix": time.time(),
               "status": "started", "binding": runner.binding}
    attempt_path = runner.root/"reports"/("phase-"+uuid.uuid4().hex+".json")
    durable_json(attempt_path, attempt)
    try:
        if args.phase == "adopt":
            output = generation_tools().adopt(runner)
        elif args.phase == "seed":
            output = runner.seed_phase()
        elif args.phase == "main":
            output = runner.main_phase()
        elif args.phase == "full":
            output = runner.full_phase(args.input)
        else:
            output = runner.path_phase(args.input, args.phase == "packets")
        attempt.update(status="review_artifact_returned_not_acceptance", output=str(output), finished_unix=time.time())
        durable_json(attempt_path, attempt, replace=True)
        print(output)
    except BaseException as error:
        attempt.update(status="paused" if isinstance(error, PauseRequested) else "failed_or_interrupted", error=repr(error), finished_unix=time.time())
        durable_json(attempt_path, attempt, replace=True)
        raise
    finally:
        runner.close()


if __name__ == "__main__":
    main()
