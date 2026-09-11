"""Explicit private-plan generation adoption. No Lightroom/original-photo access.

Loaded by the bound runner with its utilities in RUN. This module's digest is
part of the run binding. All databases opened here are private derived plans.
"""
import contextlib
import fcntl
import hashlib
import os
from pathlib import Path
import sqlite3
import stat
import struct
import time

RUN = None  # Injected by the owning runner; no second module/binding is imported.
TABLES = ("captures", "schema_objects", "tables", "rows", "entities", "issues", "packets",
          "metadata_facts", "references_out", "paths", "inventories", "family_assignments", "family_choices")


def require_runtime():
    if not hasattr(sqlite3.Connection, "setlimit"):
        raise ValueError("generation adoption requires Python 3.11+ SQLite limit API; use the reviewed Python 3.14 runtime")


class Text(bytes):
    """Preserve SQLite TEXT bytes separately from BLOB, including invalid UTF8."""


def text(value):
    return bytes(value).decode("utf-8") if isinstance(value, bytes) else value


def field_bytes(value):
    if value is None:
        return b"N", b""
    if isinstance(value, Text):
        return b"T", value
    if isinstance(value, bytes):
        return b"B", value
    if isinstance(value, int):
        return b"I", struct.pack(">q", value)
    if isinstance(value, float):
        return b"R", struct.pack(">d", value)
    raise ValueError("unexpected SQLite storage class")


def typed_scan(path, limits):
    """One ordered streaming scan of known physical tables, never retained SQL."""
    require_runtime()
    start = time.monotonic()
    summary_bytes = 0
    result = {"protocol": 1, "sqlite_version": sqlite3.sqlite_version, "tables": {},
              "captures": [], "retained_tables": {}, "rows_per_revision": {}}
    db = sqlite3.connect(path.as_uri()+"?mode=ro&immutable=1", uri=True)
    try:
        db.text_factory = Text
        db.execute("PRAGMA query_only=ON")
        db.execute("PRAGMA trusted_schema=OFF")
        db.execute("PRAGMA mmap_size=0")
        db.execute("PRAGMA cache_size=-16384")
        db.setlimit(sqlite3.SQLITE_LIMIT_LENGTH, limits["scan_row_bytes"])
        db.set_progress_handler(lambda: int(time.monotonic()-start > limits["scan_seconds"]), 10000)
        if db.execute("PRAGMA application_id").fetchone()[0] != 0x50434c49:
            raise ValueError("not an inspection plan")
        result["plan_schema"] = db.execute("PRAGMA user_version").fetchone()[0]
        actual = {text(row[0]) for row in db.execute("SELECT name FROM sqlite_schema WHERE type='table'")}
        if actual != set(TABLES):
            raise ValueError("unexpected private inspection tables")
        for table in TABLES:
            # Fixed owned table names; no schema SQL is evaluated as a program.
            cursor = db.execute(f'SELECT rowid,* FROM "{table}" ORDER BY rowid')
            columns = [column[0] for column in cursor.description]
            digest = hashlib.sha256(RUN.encoded([table, columns]))
            count = 0
            for values in cursor:
                if time.monotonic()-start > limits["scan_seconds"] or count >= limits["scan_rows"]:
                    raise ValueError("logical scan deadline/row limit")
                digest.update(b"row\0")
                row_bytes = 0
                for value in values:
                    tag, raw = field_bytes(value)
                    row_bytes += len(raw)
                    if row_bytes > limits["scan_row_bytes"]:
                        raise ValueError("logical scan row byte limit")
                    digest.update(tag+struct.pack(">Q", len(raw)))
                    # Byte-preserving digest; output never contains source values.
                    digest.update(raw)
                row = dict(zip(columns, values))
                if table == "captures":
                    if len(result["captures"]) >= limits["captures"]:
                        raise ValueError("capture-reference limit")
                    summary_bytes += sum(len(row[key]) for key in ["revision", "lineage", "path", "manifest", "stage"])
                    if summary_bytes > 8*RUN.MIB:
                        raise ValueError("logical scan summary byte limit")
                    result["captures"].append({key: text(row[key]) for key in ["revision", "lineage", "path", "manifest", "stage"]})
                elif table == "tables":
                    revision, name = text(row["revision"]), text(row["name"])
                    summary_bytes += len(row["revision"])+len(row["name"])+64
                    if summary_bytes > 8*RUN.MIB:
                        raise ValueError("logical scan summary byte limit")
                    result["retained_tables"].setdefault(revision, {})[name] = row["retained"]
                elif table == "rows":
                    revision = text(row["revision"])
                    result["rows_per_revision"][revision] = result["rows_per_revision"].get(revision, 0)+1
                count += 1
            result["tables"][table] = {"rows": count, "sha256": digest.hexdigest()}
        return result
    finally:
        db.close()


def artifact_state(directory):
    found = {}
    for entry in directory.iterdir():
        if entry.name not in {"inspection.sqlite3", "inspection.sqlite3-wal", "inspection.sqlite3-shm", "inspection.sqlite3-journal"}:
            raise ValueError("unexpected private-plan artifact")
        before = entry.lstat()
        if not stat.S_ISREG(before.st_mode):
            raise ValueError("nonregular private-plan artifact")
        found[entry.name] = list(RUN.revision(before))
    if "inspection.sqlite3" not in found:
        raise ValueError("missing private-plan database")
    for suffix in ["-wal", "-journal"]:
        value = found.get("inspection.sqlite3"+suffix)
        if value and value[2]:
            raise ValueError("nonempty WAL/journal requires separately reviewed native capture/recovery")
    return found


def copy_artifact(source, destination, expected, limits):
    start = time.monotonic()
    descriptor = os.open(source, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode) or list(RUN.revision(before)) != expected:
            raise ValueError("copy source identity changed")
        if before.st_size > limits["plan_bytes"]:
            raise ValueError("private artifact size limit")
        digest = hashlib.sha256()
        with os.fdopen(os.dup(descriptor), "rb") as reader, destination.open("xb") as writer:
            remaining = before.st_size
            while remaining:
                if time.monotonic()-start > limits["copy_seconds"]:
                    raise ValueError("private copy deadline")
                chunk = reader.read(min(RUN.MIB, remaining))
                if not chunk:
                    raise ValueError("private copy truncated")
                writer.write(chunk)
                digest.update(chunk)
                remaining -= len(chunk)
            if reader.read(1):
                raise ValueError("private copy source grew")
            writer.flush()
            os.fsync(writer.fileno())
        if (list(RUN.revision(os.fstat(descriptor))) != expected
                or list(RUN.revision(source.lstat())) != expected
                or RUN.file_sha(destination) != digest.hexdigest()):
            raise ValueError("private copy identity/digest mismatch")
        return {"bytes": before.st_size, "sha256": digest.hexdigest(), "source_revision": expected}
    finally:
        os.close(descriptor)


def old_lock(runner, request):
    old = RUN.no_links(request["predecessor"])
    if old.parent != runner.root.parent or old == runner.root:
        raise ValueError("generation requires a separate private sibling predecessor")
    for key in ["catalog_root", "original_root"]:
        original = RUN.no_links(runner.config[key])
        if old == original or old in original.parents or original in old.parents:
            raise ValueError("generation predecessor overlaps originals")
    descriptor = os.open(RUN.no_links(old/"runner.lock"), os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        if not stat.S_ISREG(os.fstat(descriptor).st_mode):
            raise ValueError("invalid predecessor lock")
        fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return old, descriptor
    except BaseException:
        os.close(descriptor)
        raise


def checked_record(old, reference, binding):
    record = RUN.evidence_document(old, reference)
    if (reference["path"] != f"commands/{record['sequence']:09d}/result.json"
            or record["source_binding"] != binding):
        raise ValueError("predecessor command binding mismatch")
    return record


def checked_value(old, record):
    if record.get("exit_code") != 0 or record.get("failure") or record.get("log_errors"):
        raise ValueError("failed command cannot establish generation admission")
    return RUN.evidence_document(old, record["stdout"])


def verify_pages(old, binding, references, revision, page_limit):
    after = 0
    total = 0
    counts = {}
    content = hashlib.sha256()
    identity = hashlib.sha256()
    for reference in references:
        record = checked_record(old, reference, binding)
        if record["requested_arguments"] != ["rows", str(old/"plans/main"), revision, "--after", str(after), "--limit", str(page_limit)]:
            raise ValueError("saved row page command/cursor differs")
        values = checked_value(old, record)
        if not isinstance(values, list) or not values or len(values) > page_limit:
            raise ValueError("invalid nonempty retained page")
        for value in values:
            sequence = value["sequence"]
            if type(sequence) is not int or sequence <= after or value.get("revision_id") != revision or not value.get("source_id"):
                raise ValueError("saved row page identity/order differs")
            after = sequence
            total += 1
            counts[value["table"]] = counts.get(value["table"], 0)+1
            content.update(RUN.encoded(value))
            identity.update(RUN.encoded([sequence, value.get("source_id"), value.get("revision_id"), value.get("table")]))
    return {"rows": total, "counts": counts, "last_sequence": after,
            "content_sha256": content.hexdigest(), "identity_sha256": identity.hexdigest()}


def adopt(runner):
    require_runtime()
    request = runner.config["generation_request"]
    if request and request.get("protocol") in (2, 3):
        return repair_adopt(runner)
    if not request or request.get("protocol") != 1:
        raise ValueError("reviewed generation request required")
    if (runner.root/"reports/generation-adoption.json").exists():
        raise ValueError("generation already adopted; use main, never repeat adoption")
    if (runner.root/"reports/generation-started.json").exists():
        raise RUN.InterruptedOperation("uncompleted generation adoption requires review; no automatic retry")
    if os.path.lexists(runner.root/"pause-request"):
        raise RUN.PauseRequested("pause before private generation adoption")
    old, descriptor = old_lock(runner, request)
    try:
        binding = RUN.evidence_document(old, request["binding"])
        journal = RUN.evidence_document(old, request["journal"])
        pause = RUN.evidence_document(old, request["pause"])
        if (request["binding"]["path"] != "binding.json" or request["journal"]["path"] != "journal.json"
                or request["pause"]["path"] != "pause-request" or not pause.get("owner")
                or journal["next_command"] != request["expected_next_command"]):
            raise ValueError("unexpected predecessor checkpoint")
        limits = request["limits"]
        if set(limits) != {"plan_bytes", "copy_seconds", "scan_seconds", "scan_rows", "scan_row_bytes", "commands", "captures"} or any(type(v) is not int or v <= 0 for v in limits.values()):
            raise ValueError("invalid generation resource limits")
        if journal["next_command"] > limits["commands"]+1:
            raise ValueError("generation command limit")
        inventory_record = checked_record(old, request["inventory_result"], binding)
        inventory = checked_value(old, inventory_record)
        if inventory_record["requested_arguments"] != ["discover", runner.config["catalog_root"]] or not inventory.get("complete"):
            raise ValueError("invalid predecessor whole inventory")
        prefix = {}
        for sequence in range(1, journal["next_command"]):
            relative = f"commands/{sequence:09d}/result.json"
            path = RUN.private_child(old, relative)
            reference = {"path": relative, "sha256": RUN.file_sha(path)}
            record = checked_record(old, reference, binding)
            if record["sequence"] != sequence:
                raise ValueError("noncontiguous predecessor journal")
            step_key = RUN.sha(RUN.encoded(record["key"]))
            step = RUN.read_json(RUN.private_child(old, "steps/"+step_key+".json"))
            if step != {"record": relative, "sequence": sequence} or step_key in prefix:
                raise ValueError("predecessor step key mismatch/duplicate")
            prefix[step_key] = reference
        if RUN.sha(RUN.encoded(prefix)) != request["command_index_sha256"]:
            raise ValueError("predecessor command index differs from reviewed immutable prefix")
        completed = {}
        for reference in request["completed_outcomes"]:
            outcome = RUN.evidence_document(old, reference)
            if outcome.get("status") != "inspected_with_reported_limits" or "inspection" not in outcome:
                raise ValueError("cannot adopt incomplete outcome")
            key = outcome["key"]
            if key in completed or RUN.candidate_key(outcome["candidate"]) != key:
                raise ValueError("duplicate/incorrect completed candidate")
            completed[key] = {"reference": reference, "outcome": outcome}
        active = request["active"]
        if active["candidate_key"] in completed or type(active["page_next"]) is not int or active["page_next"] < 1:
            raise ValueError("invalid active prefix")
        for page in range(active["page_next"]):
            key = [["main", active["candidate_key"]], active["revision"], "rows", page]
            if RUN.sha(RUN.encoded(key)) not in prefix:
                raise ValueError("active page prefix has a gap")
        next_key = [["main", active["candidate_key"]], active["revision"], "rows", active["page_next"]]
        if RUN.sha(RUN.encoded(next_key)) in prefix:
            raise ValueError("active next page already exists")
        active_references = [prefix[RUN.sha(RUN.encoded([["main", active["candidate_key"]], active["revision"], "rows", number]))] for number in range(active["page_next"])]
        active_proof = verify_pages(old, binding, active_references, active["revision"], runner.config["page_limit"])
        if active_proof["last_sequence"] != active["after"]:
            raise ValueError("active cursor differs from retained page prefix")
        source = RUN.no_links(old/"plans/main")
        state = artifact_state(source)
        if state != request["expected_source_state"]:
            raise ValueError("predecessor plan differs from reviewed filesystem revision")
        runner.space(3*sum(value[2] for value in state.values())+runner.config["minimum_free_bytes"])
        runner.summary("generation-started", {"request_sha256": RUN.sha(RUN.encoded(request)), "source_state": state, "source_binding": runner.binding})
        raw = runner.root/"plans/generation-raw"
        plan = runner.root/"plans/main"
        raw.mkdir(mode=0o700)
        plan.mkdir(mode=0o700)
        copies = {name: copy_artifact(source/name, raw/name, identity, limits) for name, identity in state.items()}
        if artifact_state(source) != state:
            raise ValueError("predecessor companions/revision changed during copy")
        working = copy_artifact(raw/"inspection.sqlite3", plan/"inspection.sqlite3", list(RUN.revision((raw/"inspection.sqlite3").stat())), limits)
        if working["sha256"] != copies["inspection.sqlite3"]["sha256"]:
            raise ValueError("working copy differs from raw source")
        RUN.sync_dir(raw)
        RUN.sync_dir(plan)
        RUN.sync_dir(plan.parent)
        runner.summary("generation-copy", {"copies": copies, "working_copy": working, "source_state": state})
        before = typed_scan(plan/"inspection.sqlite3", limits)
        if (before["plan_schema"] != 1 or before["retained_tables"] != request["expected_table_counts"]
                or before["rows_per_revision"] != request["expected_rows"]
                or {item["revision"] for item in before["captures"]} != set(request["expected_rows"])
                or before["tables"]["family_choices"]["rows"] != 0):
            raise ValueError("copied plan differs from reviewed counts/revisions or contains choices")
        captures = {item["revision"]: item for item in before["captures"]}
        completed_revisions = {item["outcome"]["inspection"]["revision"] for item in completed.values()}
        if len(completed_revisions) != len(completed) or completed_revisions | {active["revision"]} != set(captures):
            raise ValueError("completed/active outcomes do not account for every copied revision")
        inventory_candidates = {RUN.candidate_key(item): item for item in inventory["candidates"]}
        if any(count > before["retained_tables"][active["revision"]].get(name, -1) for name,count in active_proof["counts"].items()):
            raise ValueError("active saved prefix exceeds retained table counts")
        candidate_for_revision = {item["outcome"]["inspection"]["revision"]: key for key,item in completed.items()} | {active["revision"]: active["candidate_key"]}
        for key, saved in completed.items():
            outcome = saved["outcome"]
            inspection = outcome["inspection"]
            revision = inspection["revision"]
            capture = captures[revision]
            if (outcome["candidate"] != inventory_candidates.get(key)
                    or outcome["capture_path"] != str(RUN.native_path(RUN.json.loads(capture["path"])))
                    or inspection["capture_path"] != outcome["capture_path"]):
                raise ValueError("completed outcome capture/inventory mismatch")
            report_ref = next((value for value in prefix.values() if value["path"] == f"commands/{inspection['report']:09d}/result.json"), None)
            if report_ref is None:
                raise ValueError("completed report is not in the retained command prefix")
            report_record = checked_record(old, report_ref, binding)
            report = checked_value(old, report_record)
            if (report_record["requested_arguments"] != ["report", str(old/"plans/main"), revision]
                    or report["revision_id"] != revision or report["lineage_id"] != capture["lineage"]
                    or {item["name"]:item["retained"] for item in report["tables"]} != before["retained_tables"][revision]
                    or inspection["stage"] != report["stage"] or report["stage"] != capture["stage"]):
                raise ValueError("completed outcome differs from retained plan/report")
            summary = inspection["pages"]["rows"]
            references = [next((value for value in prefix.values() if value["path"] == f"commands/{sequence:09d}/result.json"), None) for sequence in summary["pages"]]
            if any(value is None for value in references):
                raise ValueError("completed row page is not in retained prefix")
            proof = verify_pages(old, binding, references, revision, runner.config["page_limit"])
            for field in ["rows", "counts", "last_sequence", "content_sha256", "identity_sha256"]:
                if proof[field] != summary[field]:
                    raise ValueError("completed page summary differs from actual saved bytes")
            if proof["counts"] != {name:count for name,count in before["retained_tables"][revision].items() if count} or proof["rows"] != before["rows_per_revision"][revision]:
                raise ValueError("completed saved page counts differ from retained plan")
        for capture in before["captures"]:
            path = RUN.native_path(RUN.json.loads(capture["path"]))
            if path.parent.parent != old.parent and old.parent not in path.parents:
                raise ValueError("capture reference leaves owned private results")
            stored = RUN.json.loads(capture["manifest"])
            candidate = inventory_candidates.get(candidate_for_revision[capture["revision"]])
            if candidate is None or stored["request"]["source"] != candidate["path"]:
                raise ValueError("retained capture source differs from candidate identity")
            if RUN.read_json(RUN.no_links(path/"manifest.json")) != stored:
                raise ValueError("retained capture manifest changed")
        runner.summary("generation-before", before)
        # The real bundled-Rust CLI performs the index transaction on only this copy.
        upgraded = runner.require(["generation", "upgrade"], ["rows", plan, active["revision"], "--after", active["after"], "--limit", 1])
        # A normal close checkpoints WAL; reject rather than ignore a live WAL.
        artifact_state(plan)
        after = typed_scan(plan/"inspection.sqlite3", limits)
        runner.summary("generation-after", after)
        if after["plan_schema"] != 2 or {k:v for k,v in before.items() if k != "plan_schema"} != {k:v for k,v in after.items() if k != "plan_schema"}:
            raise ValueError("schema upgrade changed retained logical data")
        with contextlib.closing(sqlite3.connect((plan/"inspection.sqlite3").as_uri()+"?mode=ro&immutable=1", uri=True)) as db:
            if db.execute("PRAGMA user_version").fetchone()[0] != 2:
                raise ValueError("upgrade did not publish schema2")
        if artifact_state(source) != state:
            raise ValueError("predecessor changed during adoption")
        receipt = {"protocol": 1, "request_sha256": RUN.sha(RUN.encoded(request)), "source_binding": runner.binding,
                   "predecessor": str(old), "predecessor_binding": binding, "predecessor_state": state,
                   "prefix": prefix, "completed": completed, "active": active, "active_verified_prefix": active_proof, "original_inventory": inventory,
                   "copies": copies, "working_copy": working, "before_sha256": RUN.file_sha(runner.root/"reports/generation-before.json"),
                   "after_sha256": RUN.file_sha(runner.root/"reports/generation-after.json"),
                   "after_logical_state_equal": True, "upgrade_command": upgraded["record"]["sequence"],
                   "automatic_selection": False, "migration_executed": False}
        return runner.summary("generation-adoption", receipt)
    finally:
        os.close(descriptor)


class Replay:
    def __init__(self, runner):
        self.runner = runner
        self.request = runner.config["generation_request"]
        self.receipt = RUN.read_json(runner.root/"reports/generation-adoption.json")
        if (self.receipt["request_sha256"] != RUN.sha(RUN.encoded(self.request))
                or self.receipt["source_binding"] != runner.binding or not self.receipt["after_logical_state_equal"]):
            raise ValueError("generation adoption binding mismatch")
        self.old, self.descriptor = old_lock(runner, self.request)
        try:
            RUN.evidence_document(self.old, self.request["journal"])
            RUN.evidence_document(self.old, self.request["pause"])
            if artifact_state(self.old/"plans/main") != self.receipt["predecessor_state"]:
                raise ValueError("frozen predecessor plan changed")
        except BaseException:
            os.close(self.descriptor)
            raise

    def close(self):
        os.close(self.descriptor)

    def result(self, reference):
        record = checked_record(self.old, reference, self.receipt["predecessor_binding"])
        artifact = record.get("stdout")
        value = None
        if artifact:
            path = RUN.private_child(self.old, artifact["path"])
            if RUN.file_sha(path) != artifact["sha256"]:
                raise ValueError("adopted stdout changed")
            try:
                value = RUN.read_json(path, record["stdout_cap"])
            except (ValueError, UnicodeError):
                pass
        # This is explicitly a reference wrapper, never a fabricated child result.
        wrapper = dict(record, sequence=f"adopted/{record['sequence']:09d}",
                       kind="adopted_reference", predecessor=str(self.old), original_record=reference)
        return {"ok": record["exit_code"] == 0 and not record["failure"] and not record["log_errors"] and value is not None,
                "value": value, "record": wrapper}

    def replay(self, key, arguments):
        reference = self.receipt["prefix"].get(RUN.sha(RUN.encoded(key)))
        if reference is None:
            return None
        record = checked_record(self.old, reference, self.receipt["predecessor_binding"])
        old_plan, new_plan = str(self.old/"plans/main"), str(self.runner.root/"plans/main")
        expected = [new_plan if value == old_plan else value for value in record["requested_arguments"]]
        if record["key"] != key or expected != [str(value) for value in arguments]:
            raise ValueError("adopted arguments differ outside exact plan mapping")
        return self.result(reference)

    def document(self, sequence, command):
        number = int(sequence.removeprefix("adopted/"))
        reference = next((item for item in self.receipt["prefix"].values() if item["path"] == f"commands/{number:09d}/result.json"), None)
        if reference is None:
            raise ValueError("unknown adopted command")
        result = self.result(reference)
        if not result["ok"] or result["record"]["requested_arguments"][0] != command:
            raise ValueError("invalid adopted document")
        return result["value"]

    def outcome(self, key, candidate):
        saved = self.receipt["completed"].get(key)
        if saved is None:
            return None
        original = RUN.evidence_document(self.old, saved["reference"])
        if original != saved["outcome"] or original["candidate"] != candidate:
            raise ValueError("completed adopted outcome changed")
        result = RUN.json.loads(RUN.json.dumps(original))
        inspection = result["inspection"]
        inspection["report"] = f"adopted/{inspection['report']:09d}"
        for page in inspection["pages"].values():
            page["pages"] = [f"adopted/{sequence:09d}" for sequence in page["pages"]]
        if "capture_command" in result:
            result["capture_command"] = f"adopted/{result['capture_command']:09d}"
        result["adopted_outcome"] = {"predecessor": str(self.old), "reference": saved["reference"]}
        return result


def admit_inventory(runner):
    """A persistent barrier covers journal reservation even before step publication."""
    current = runner.root/"reports/generation-admission-current.json"
    if current.exists():
        previous_path = RUN.private_child(runner.root, RUN.read_json(current)["attempt"])
        previous = RUN.read_json(previous_path)
        state = previous["status"]
        if previous.get("source_binding") != runner.binding:
            raise RUN.InterruptedOperation("previous generation admission source binding differs")
        if state not in {"complete", "paused_before_discovery", "paused_before_registration"}:
            raise RUN.InterruptedOperation("previous generation admission failed/unresolved; review its preserved command/state before another attempt")
        if state.startswith("paused_before_"):
            key = previous["discovery_key"] if state == "paused_before_discovery" else previous["registration_key"]
            if (runner.root/"steps"/(RUN.sha(RUN.encoded(key))+".json")).exists():
                raise RUN.InterruptedOperation("paused admission unexpectedly reserved a command")
            if RUN.read_json(runner.root/"journal.json")["next_command"] != previous["pause_next_command"]:
                raise RUN.InterruptedOperation("paused admission has an unresolved journal reservation")
            if state == "paused_before_registration":
                runner.command_document(previous["discovery_command"], "discover")
                if not RUN.unchanged_inventory(previous["inventory_delta"]):
                    raise RUN.InterruptedOperation("paused registration lacks a valid inventory admission")
        else:
            # Successful progression must be backed by both actual successful
            # commands, not only a mutable status string.
            runner.command_document(previous["discovery_command"], "discover")
            runner.command_document(previous["registration_command"], "register-inventory")
            if not RUN.unchanged_inventory(previous["inventory_delta"]):
                raise RUN.InterruptedOperation("completed admission lacks unchanged inventory proof")
    if os.path.lexists(runner.root/"pause-request"):
        raise RUN.PauseRequested("coordinator pause before generation admission")
    identifier = RUN.uuid.uuid4().hex
    path = runner.root/"reports"/("generation-admission-"+identifier+".json")
    attempt = {"status":"started", "source_binding":runner.binding,
               "discovery_key":["generation","admission",identifier,"discover"],
               "registration_key":["generation","admission",identifier,"register"],
               "started_unix":time.time()}
    RUN.durable_json(path, attempt)
    RUN.durable_json(current, {"attempt":str(path.relative_to(runner.root))}, replace=current.exists())
    phase = "discovery"
    try:
        inventory = runner.require(attempt["discovery_key"], ["discover",runner.config["catalog_root"]])
        attempt["discovery_command"] = inventory["record"]["sequence"]
        delta = RUN.inventory_delta(runner.generation.receipt["original_inventory"], inventory["value"])
        attempt["inventory_delta"] = delta
        RUN.durable_json(path, attempt, replace=True)
        if not RUN.unchanged_inventory(delta):
            raise ValueError("whole inventory changed since adopted generation")
        phase = "registration"
        registered = runner.require(attempt["registration_key"], ["register-inventory",runner.root/"plans/main",runner.root/inventory["record"]["stdout"]["path"]])
        attempt.update(status="complete", registration_command=registered["record"]["sequence"], finished_unix=time.time())
        RUN.durable_json(path, attempt, replace=True)
        return inventory
    except RUN.PauseRequested:
        # call() raises this only before reserving/starting a new command. Verify
        # that boundary rather than classifying a hard-crash orphan as a pause.
        key = attempt["discovery_key"] if phase == "discovery" else attempt["registration_key"]
        if (runner.root/"steps"/(RUN.sha(RUN.encoded(key))+".json")).exists():
            attempt.update(status="failed", error="pause crossed command reservation", finished_unix=time.time())
        else:
            attempt.update(status="paused_before_"+phase, pause_next_command=RUN.read_json(runner.root/"journal.json")["next_command"], finished_unix=time.time())
        RUN.durable_json(path, attempt, replace=True)
        raise
    except BaseException as error:
        attempt.update(status="failed", error=repr(error), phase=phase, finished_unix=time.time())
        RUN.durable_json(path, attempt, replace=True)
        raise


def private_evidence(base, reference):
    """A pinned coordinator document in the same owned private-results tree."""
    if set(reference) != {"path", "sha256"}:
        raise ValueError("invalid coordinator evidence reference")
    path = RUN.no_links(reference["path"])
    if not Path(reference["path"]).is_absolute() or base not in path.parents:
        raise ValueError("coordinator evidence leaves private-results tree")
    meta = path.lstat()
    if not stat.S_ISREG(meta.st_mode) or meta.st_size > 16*RUN.MIB:
        raise ValueError("coordinator evidence size/type")
    value = RUN.read_json(path)
    if RUN.file_sha(path) != reference["sha256"] or RUN.revision(path.lstat()) != RUN.revision(meta):
        raise ValueError("coordinator evidence changed")
    return value


def failed_checkpoint(old, request, binding):
    evidence = request["failed_predecessor"]
    failed = private_evidence(old.parent, evidence["result"])
    phase = private_evidence(old.parent, evidence["phase"])
    review = private_evidence(old.parent, evidence["ownership_review"])
    control = private_evidence(old.parent, evidence["control"])
    result_path = Path(evidence["result"]["path"])
    if (result_path.name != "result.json" or result_path.parent.parent.name != "attempts"
            or Path(evidence["control"]["path"]) != result_path.parent.parent.parent/"current.json"
            or control != {"attempt_id": result_path.parent.name}):
        raise ValueError("failed control/attempt mismatch")
    cleanup = failed.get("cleanup") or {}
    if (failed.get("status") != "failed_or_unknown" or failed.get("exit_code") in (None, 0)
            or failed.get("failure") != "RuntimeError('sampled observed RSS stop threshold exceeded')"
            or cleanup.get("root_reaped") is not True or cleanup.get("remaining_observed") != []
            or cleanup.get("errors") != [] or cleanup.get("ownership_uncertainties") != []
            or phase.get("status") != "failed_or_interrupted" or phase.get("phase") != "main"
            or phase.get("binding") != binding or phase.get("started_unix") < failed.get("started_unix")
            or Path(evidence["phase"]["path"]).parent != old/"reports"):
        raise ValueError("failed predecessor is not the reviewed reaped memory interruption")
    if (review.get("status") != "PASS" or review.get("failed_result_sha256") != evidence["result"]["sha256"]
            or review.get("phase_sha256") != evidence["phase"]["sha256"]
            or review.get("next_command") != request["expected_next_command"]
            or review.get("known_processes_absent") is not True
            or review.get("no_unresolved_command") is not True):
        raise ValueError("independent failed-predecessor ownership review missing/mismatched")
    if os.path.lexists(old/"pause-request"):
        raise ValueError("failed predecessor unexpectedly has a pause; no fabricated paused state")
    if RUN.evidence_document(old, request["journal"]) != {"next_command": request["expected_next_command"]}:
        raise ValueError("failed predecessor journal changed")
    return {"failure_classification_retained": failed["status"],
            "supervisor_ownership_retained": failed.get("ownership_status"),
            "review": evidence["ownership_review"], "scope": "reviewed known processes and terminal commands, not global process absence"}


def local_prefix(old, binding, request):
    limit = request["limits"]["commands"]
    count = request["expected_next_command"]-(2 if request.get("protocol") == 3 else 1)
    if type(count) is not int or count < 1 or count > limit:
        raise ValueError("repair command count limit")
    prefix = {}
    for sequence in range(1, count+1):
        relative = f"commands/{sequence:09d}/result.json"
        reference = {"path": relative, "sha256": RUN.file_sha(RUN.private_child(old, relative))}
        record = checked_record(old, reference, binding)
        if record["sequence"] != sequence or record.get("exit_code") != 0 or record.get("failure") or record.get("log_errors"):
            raise ValueError("failed/unresolved command cannot enter repair prefix")
        key = RUN.sha(RUN.encoded(record["key"]))
        step = RUN.read_json(RUN.private_child(old, "steps/"+key+".json"))
        if step != {"record": relative, "sequence": sequence} or key in prefix:
            raise ValueError("repair prefix duplicate/key/step mismatch")
        prefix[key] = reference
    if RUN.sha(RUN.encoded(prefix)) != request["command_index_sha256"]:
        raise ValueError("repair command prefix differs")
    return prefix


class RepairOrigins:
    """Bounded explicit v4-local/v2-inherited record namespaces; no stored code."""
    def __init__(self, runner, old, binding, request, prefix):
        self.roots = {"local": {"root": str(old), "binding": binding, "prefix": prefix,
                                 "state": request["expected_source_state"]}}
        self.descriptors = []
        inherited = RUN.evidence_document(old, request["inherited_adoption"])
        if (inherited.get("protocol") != 1 or inherited.get("source_binding") != binding
                or inherited.get("after_logical_state_equal") is not True):
            raise ValueError("inherited adoption lineage unsupported or unverified")
        ancestor = RUN.no_links(inherited["predecessor"])
        if ancestor == old:
            raise ValueError("inherited lineage cycle")
        root, descriptor = old_lock(runner, {"predecessor": str(ancestor)})
        self.descriptors.append(descriptor)
        try:
            if (RUN.read_json(root/"binding.json") != inherited["predecessor_binding"]
                    or artifact_state(root/"plans/main") != inherited["predecessor_state"]
                    or len(inherited["prefix"]) > request["limits"]["commands"]):
                raise ValueError("inherited source state/binding/size changed")
            self.roots["inherited"] = {"root": str(root), "binding": inherited["predecessor_binding"],
                                       "prefix": inherited["prefix"], "state": inherited["predecessor_state"]}
            # Validate all index descriptors before resolving individual commands.
            for descriptor in self.roots.values():
                for key, reference in descriptor["prefix"].items():
                    record = checked_record(Path(descriptor["root"]), reference, descriptor["binding"])
                    if (key != RUN.sha(RUN.encoded(record["key"])) or record.get("exit_code") != 0
                            or record.get("failure") or record.get("log_errors")):
                        raise ValueError("inherited prefix key/failure mismatch")
        except BaseException:
            self.close()
            raise

    def close(self):
        for descriptor in self.descriptors:
            os.close(descriptor)
        self.descriptors = []

    def reference(self, sequence):
        if type(sequence) is int and sequence > 0:
            origin, number = "local", f"{sequence:09d}"
        elif isinstance(sequence, str) and sequence.startswith("adopted/"):
            parts = sequence.split("/")
            if len(parts) == 2:
                origin, number = "inherited", parts[1]
            elif len(parts) == 3 and parts[1] in self.roots:
                origin, number = parts[1:]
            else:
                raise ValueError("unsupported inherited command identity")
            if len(number) != 9 or not number.isascii() or not number.isdigit():
                raise ValueError("invalid inherited sequence token")
        else:
            raise ValueError("invalid command identity")
        relative = f"commands/{number}/result.json"
        matches = [ref for ref in self.roots[origin]["prefix"].values() if ref["path"] == relative]
        if len(matches) != 1:
            raise ValueError("missing/ambiguous inherited command")
        return {"origin": origin, "reference": matches[0]}

    def load(self, reference):
        descriptor = self.roots[reference["origin"]]
        root = Path(descriptor["root"])
        record = checked_record(root, reference["reference"], descriptor["binding"])
        return root, record

    def unchanged(self):
        for descriptor in self.roots.values():
            root = Path(descriptor["root"])
            if (artifact_state(root/"plans/main") != descriptor["state"]
                    or RUN.read_json(root/"binding.json") != descriptor["binding"]):
                raise ValueError("repair origin changed")


def repair_page(origins, reference, revision, limit, after, counts, content, identity):
    root, record = origins.load(reference)
    if record["requested_arguments"] != ["rows", str(root/"plans/main"), revision, "--after", str(after), "--limit", str(limit)]:
        raise ValueError("repair page command/cursor differs")
    values = checked_value(root, record)
    if not isinstance(values, list) or not values or len(values) > limit:
        raise ValueError("invalid repair page")
    for value in values:
        sequence = value["sequence"]
        if type(sequence) is not int or sequence <= after or value.get("revision_id") != revision or not value.get("source_id"):
            raise ValueError("repair page identity/order differs")
        after = sequence
        counts[value["table"]] = counts.get(value["table"], 0)+1
        RUN.update_canonical_hash(content, value)
        RUN.update_canonical_hash(identity, [sequence, value.get("source_id"), value.get("revision_id"), value.get("table")])
    return after, len(values)


def repair_rows(origins, sequences, revision, limit):
    counts = {}; total = 0; after = 0
    content = hashlib.sha256(); identity = hashlib.sha256()
    for sequence in sequences:
        after, count = repair_page(origins, origins.reference(sequence), revision, limit, after, counts, content, identity)
        total += count
    return {"rows": total, "counts": counts, "last_sequence": after,
            "content_sha256": content.hexdigest(), "identity_sha256": identity.hexdigest()}


def repair_adopt(runner):
    request = runner.config["generation_request"]
    protocol = request["protocol"]
    if protocol == 3 and request.get("page_limit") != runner.config["page_limit"]:
        raise ValueError("PID repair page limit differs from frozen runner")
    checkpoint = pid_failed_checkpoint if protocol == 3 else failed_checkpoint
    origins_type = PidRepairOrigins if protocol == 3 else RepairOrigins
    if (runner.root/"reports/generation-adoption.json").exists() or (runner.root/"reports/generation-started.json").exists():
        raise RUN.InterruptedOperation("repair generation already attempted; review instead of retry")
    limits = request["limits"]
    required = {"plan_bytes", "copy_seconds", "scan_seconds", "scan_rows", "scan_row_bytes", "commands", "captures"}
    if set(limits) != required or any(type(v) is not int or v <= 0 for v in limits.values()):
        raise ValueError("invalid repair limits")
    old, descriptor = old_lock(runner, request)
    origins = None
    try:
        runner.summary("generation-started", {"protocol": protocol, "request_sha256": RUN.sha(RUN.encoded(request)),
                                               "status": "pending_repair_verification", "source_binding": runner.binding})
        binding = RUN.evidence_document(old, request["binding"])
        if request["binding"]["path"] != "binding.json" or request["journal"]["path"] != "journal.json":
            raise ValueError("repair binding/journal locator")
        failure_proof = checkpoint(old, request, binding)
        prefix = local_prefix(old, binding, request)
        origins = origins_type(runner, old, binding, request, prefix)
        origins.unchanged()
        inventory_record = checked_record(old, request["inventory_result"], binding)
        inventory = checked_value(old, inventory_record)
        if inventory_record["requested_arguments"] != ["discover", runner.config["catalog_root"]] or inventory.get("complete") is not True:
            raise ValueError("invalid repair inventory")
        candidates = {RUN.candidate_key(item): item for item in inventory["candidates"]}
        if len(candidates) != len(inventory["candidates"]):
            raise ValueError("duplicate repair inventory candidate")
        completed = {}
        for reference in request["completed_outcomes"]:
            outcome = RUN.evidence_document(old, reference)
            key = outcome["key"]
            if key in completed or outcome.get("status") != "inspected_with_reported_limits" or outcome["candidate"] != candidates.get(key):
                raise ValueError("invalid completed repair outcome")
            completed[key] = {"reference": reference, "outcome": outcome}
        active = request["active"]
        if (active["candidate_key"] in completed or type(active["page_next"]) is not int
                or not 1 <= active["page_next"] <= len(prefix)):
            raise ValueError("invalid active repair prefix")
        active_sequences = []
        for number in range(active["page_next"]+1):
            key = RUN.sha(RUN.encoded([["main", active["candidate_key"]], active["revision"], "rows", number]))
            reference = prefix.get(key)
            if number == active["page_next"]:
                if reference is not None:
                    raise ValueError("active repair next page already exists")
            else:
                if reference is None:
                    raise ValueError("active repair prefix gap")
                record = checked_record(old, reference, binding)
                active_sequences.append(record["sequence"])
        active_proof = repair_rows(origins, active_sequences, active["revision"], runner.config["page_limit"])
        if active_proof["last_sequence"] != active["after"]:
            raise ValueError("repair active cursor differs")
        source = old/"plans/main"; state = artifact_state(source)
        if state != request["expected_source_state"]:
            raise ValueError("repair source state differs")
        runner.space(sum(value[2] for value in state.values())+runner.config["minimum_free_bytes"])
        plan = runner.root/"plans/main"; plan.mkdir(mode=0o700)
        copies = {name: copy_artifact(source/name, plan/name, identity, limits) for name, identity in state.items()}
        origins.unchanged()
        RUN.sync_dir(plan); RUN.sync_dir(plan.parent)
        copied_state = artifact_state(plan)
        runner.summary("generation-copy", {"protocol": protocol, "copies": copies, "source_state": state, "copied_state": copied_state})
        scan = typed_scan(plan/"inspection.sqlite3", limits)
        if (scan["plan_schema"] != 2 or scan["retained_tables"] != request["expected_table_counts"]
                or scan["rows_per_revision"] != request["expected_rows"] or scan["tables"]["family_choices"]["rows"] != 0):
            raise ValueError("repair schema/count/choice mismatch")
        captures = {item["revision"]: item for item in scan["captures"]}
        revisions = {item["outcome"]["inspection"]["revision"] for item in completed.values()}
        if len(revisions) != len(completed) or revisions | {active["revision"]} != set(captures) or set(captures) != set(request["expected_rows"]):
            raise ValueError("repair outcomes do not account for every capture")
        if any(count > scan["retained_tables"][active["revision"]].get(table, -1) for table, count in active_proof["counts"].items()):
            raise ValueError("repair active prefix exceeds retained data")
        for saved in completed.values():
            outcome = saved["outcome"]; inspection = outcome["inspection"]; revision = inspection["revision"]
            origin, record = origins.load(origins.reference(inspection["report"]))
            report = checked_value(origin, record)
            if (record["requested_arguments"] != ["report", str(origin/"plans/main"), revision]
                    or report["revision_id"] != revision or report["lineage_id"] != captures[revision]["lineage"]
                    or {row["name"]:row["retained"] for row in report["tables"]} != scan["retained_tables"][revision]
                    or report["stage"] != inspection["stage"] or report["stage"] != captures[revision]["stage"]):
                raise ValueError("repair completed report differs from copied data")
            summary = inspection["pages"]["rows"]
            proof = repair_rows(origins, summary["pages"], revision, runner.config["page_limit"])
            if any(proof[field] != summary[field] for field in proof) or proof["rows"] != scan["rows_per_revision"][revision] or proof["counts"] != {k:v for k,v in scan["retained_tables"][revision].items() if v}:
                raise ValueError("repair completed pages/counts differ")
        keys = {item["outcome"]["inspection"]["revision"]:key for key,item in completed.items()} | {active["revision"]:active["candidate_key"]}
        for revision, capture in captures.items():
            path = RUN.no_links(RUN.native_path(RUN.json.loads(capture["path"])))
            if old.parent not in path.parents:
                raise ValueError("repair capture outside private results")
            manifest = RUN.json.loads(capture["manifest"])
            if RUN.read_json(path/"manifest.json") != manifest or manifest["request"]["source"] != candidates[keys[revision]]["path"]:
                raise ValueError("repair capture manifest/source mismatch")
            if revision in revisions:
                outcome = completed[keys[revision]]["outcome"]
                if outcome["capture_path"] != str(path) or outcome["inspection"]["capture_path"] != str(path):
                    raise ValueError("repair completed capture path mismatch")
        runner.summary("generation-reconciled", scan)
        origins.unchanged(); checkpoint(old, request, binding)
        if artifact_state(plan) != copied_state:
            raise ValueError("repair owned copy changed during verification")
        return runner.summary("generation-adoption", {"protocol": protocol, "request_sha256": RUN.sha(RUN.encoded(request)),
            "source_binding": runner.binding, "predecessor": str(old), "predecessor_binding": binding,
            "predecessor_state": state, "origins": origins.roots, "prefix": prefix, "completed": completed,
            "active": active, "active_verified_prefix": active_proof, "original_inventory": inventory,
            "copies": copies, "reconciled_sha256": RUN.file_sha(runner.root/"reports/generation-reconciled.json"),
            "after_logical_state_equal": True, "equality_basis": "physical source-copy SHA equality plus one read-only typed copy reconciliation; no schema/data mutation",
            "failed_predecessor": request["failed_predecessor"], "failure_proof": failure_proof,
            "actual_native_commands": 0, "automatic_selection": False, "migration_executed": False})
    finally:
        if origins is not None:
            origins.close()
        os.close(descriptor)


class RepairReplay(RepairOrigins):
    def __init__(self, runner):
        self.runner = runner; self.request = runner.config["generation_request"]
        self.receipt = RUN.read_json(runner.root/"reports/generation-adoption.json")
        if (self.receipt.get("protocol") != 2 or self.receipt["request_sha256"] != RUN.sha(RUN.encoded(self.request))
                or self.receipt["source_binding"] != runner.binding or self.receipt["after_logical_state_equal"] is not True):
            raise ValueError("repair replay receipt binding mismatch")
        self.old, descriptor = old_lock(runner, self.request)
        self.descriptors = [descriptor]
        try:
            self.roots = self.receipt["origins"]
            if set(self.roots) != {"local", "inherited"} or self.roots["local"]["root"] != str(self.old):
                raise ValueError("repair replay origins differ")
            binding = RUN.evidence_document(self.old, self.request["binding"])
            inherited = RUN.evidence_document(self.old, self.request["inherited_adoption"])
            expected_local = {"root": str(self.old), "binding": binding,
                              "prefix": self.receipt["prefix"], "state": self.request["expected_source_state"]}
            expected_inherited = {"root": inherited["predecessor"], "binding": inherited["predecessor_binding"],
                                  "prefix": inherited["prefix"], "state": inherited["predecessor_state"]}
            if (self.roots != {"local": expected_local, "inherited": expected_inherited}
                    or self.receipt["predecessor_binding"] != binding
                    or self.receipt["predecessor_state"] != self.request["expected_source_state"]
                    or RUN.sha(RUN.encoded(self.receipt["prefix"])) != self.request["command_index_sha256"]
                    or inherited.get("protocol") != 1 or inherited.get("source_binding") != binding
                    or inherited.get("after_logical_state_equal") is not True
                    or RUN.no_links(inherited["predecessor"]) == self.old):
                raise ValueError("repair replay lineage/index differs")
            _, ancestor = old_lock(runner, {"predecessor": self.roots["inherited"]["root"]})
            self.descriptors.append(ancestor)
            failed_checkpoint(self.old, self.request, binding)
            self.unchanged()
        except BaseException:
            self.close(); raise

    def result(self, reference):
        root, record = self.load(reference)
        value = checked_value(root, record)
        wrapped = dict(record, sequence=f"adopted/{reference['origin']}/{record['sequence']:09d}",
                       kind="adopted_reference", predecessor=str(root), original_record=reference["reference"])
        return {"ok": True, "value": value, "record": wrapped}

    def replay(self, key, arguments):
        reference = self.receipt["prefix"].get(RUN.sha(RUN.encoded(key)))
        if reference is None:
            return None
        reference = {"origin": "local", "reference": reference}
        root, record = self.load(reference)
        expected = [str(self.runner.root/"plans/main") if item == str(root/"plans/main") else item for item in record["requested_arguments"]]
        if record["key"] != key or expected != [str(item) for item in arguments]:
            raise ValueError("repair replay arguments differ outside exact plan mapping")
        return self.result(reference)

    def document(self, sequence, command):
        result = self.result(self.reference(sequence))
        if result["record"]["requested_arguments"][0] != command:
            raise ValueError("unexpected inherited document command")
        return result["value"]

    def outcome(self, key, candidate):
        saved = self.receipt["completed"].get(key)
        if saved is None:
            return None
        original = RUN.evidence_document(self.old, saved["reference"])
        if original != saved["outcome"] or original["candidate"] != candidate:
            raise ValueError("repair completed outcome changed")
        result = RUN.json.loads(RUN.json.dumps(original))
        def qualified(sequence):
            reference = self.reference(sequence)
            _, record = self.load(reference)
            return f"adopted/{reference['origin']}/{record['sequence']:09d}"
        inspection = result["inspection"]
        inspection["report"] = qualified(inspection["report"])
        for page in inspection["pages"].values():
            page["pages"] = [qualified(sequence) for sequence in page["pages"]]
        if "capture_command" in result:
            result["capture_command"] = qualified(result["capture_command"])
        result["adopted_outcome"] = {"predecessor": str(self.old), "reference": saved["reference"]}
        return result


def pid_failed_checkpoint(old, request, binding):
    """Protocol3 admits one pinned PID-reuse stop and one interrupted rows tail."""
    evidence = request["failed_predecessor"]
    failed = private_evidence(old.parent, evidence["result"])
    phase = private_evidence(old.parent, evidence["phase"])
    review = private_evidence(old.parent, evidence["ownership_review"])
    control = private_evidence(old.parent, evidence["control"])
    result_path = Path(evidence["result"]["path"])
    if (result_path.name != "result.json" or result_path.parent.parent.name != "attempts"
            or Path(evidence["control"]["path"]) != result_path.parent.parent.parent/"current.json"
            or control != {"attempt_id": result_path.parent.name}):
        raise ValueError("PID repair failed control/attempt mismatch")
    cleanup = failed.get("cleanup") or {}
    if (failed.get("status") != "failed_or_unknown" or failed.get("exit_code") != 1
            or failed.get("ownership_status") != "unknown_requires_review"
            or failed.get("failure") != "RuntimeError('owned PID identity changed')"
            or cleanup.get("root_reaped") is not True or cleanup.get("remaining_observed") != []
            or cleanup.get("ownership_uncertainties") != [] or cleanup.get("root_signals") != ["SIGINT"]
            or cleanup.get("errors") != ["initial process observation: RuntimeError('owned PID identity changed')"]
            or phase.get("status") != "failed_or_interrupted" or phase.get("phase") != "main"
            or phase.get("error") != "InterruptedOperation('interrupted_or_launch_error: KeyboardInterrupt()')"
            or phase.get("binding") != binding
            or not failed["started_unix"] <= phase["started_unix"] <= phase["finished_unix"] <= failed["finished_unix"]
            or Path(evidence["phase"]["path"]).parent != old/"reports"):
        raise ValueError("not the reviewed PID-reuse interruption")
    tail = request["interrupted_tail"]
    record = checked_record(old, tail["record"], binding)
    prior = checked_record(old, tail["prior_record"], binding)
    process = RUN.evidence_document(old, tail["process"])
    prior_process = RUN.evidence_document(old, tail["prior_process"])
    stop = private_evidence(old.parent, tail["stop_decision"])
    root_process = private_evidence(old.parent, tail["root_process"])
    active = request["active"]
    expected_args = ["rows", str(old/"plans/main"), active["revision"], "--after", str(active["after"]), "--limit", str(request["page_limit"])]
    expected_key = [["main", active["candidate_key"]], active["revision"], "rows", active["page_next"]]
    if (record["sequence"] != request["expected_next_command"]-1 or record["sequence"] <= 1
            or record["requested_arguments"] != expected_args or record["key"] != expected_key
            or record.get("exit_code") != -9
            or record.get("failure") != "interrupted_or_launch_error: KeyboardInterrupt()" or record.get("log_errors") != []
            or prior.get("exit_code") != 0 or prior.get("failure") or prior.get("log_errors")
            or not 1 <= prior["sequence"] < record["sequence"]
            or not failed["started_unix"] <= prior["started_unix"] <= prior["finished_unix"] < record["started_unix"] <= record["finished_unix"] <= failed["finished_unix"]):
        raise ValueError("PID repair requires exact terminal interrupted read-only tail")
    for value, command, reference in [(process, record, tail["process"]), (prior_process, prior, tail["prior_process"])]:
        if (reference["path"] != f"commands/{command['sequence']:09d}/process.json"
                or type(value["pid"]) is not int or value["pid"] <= 1 or value["process_group"] != value["pid"]
                or value["argv"] != command["argv"]
                or not command["started_unix"] <= value["started_unix"] <= command["finished_unix"]):
            raise ValueError("PID repair process publication differs")
    old_lifetimes = [item for item in stop["known_owned"] if item["pid"] == process["pid"]]
    if (process["pid"] != prior_process["pid"] or len(old_lifetimes) != 1
            or old_lifetimes[0]["group"] != process["pid"]
            or old_lifetimes[0]["parent"] != stop["root_pid"] or stop["root_pid"] != root_process["pid"]
            or root_process["argv"][-2:] != ["main", str(old)]
            or root_process["started_unix"] != failed["started_unix"]
            or stop["reason"] != failed["failure"]
            or not record["started_unix"] <= stop["unix"] <= failed["finished_unix"]
            or Path(tail["stop_decision"]["path"]) != result_path.parent/"stop-decision.json"
            or Path(tail["root_process"]["path"]) != result_path.parent/"process.json"):
        raise ValueError("PID reuse root/lifetime evidence differs")
    old_start = time.mktime(time.strptime(old_lifetimes[0]["start"].strip(), "%a %b %d %H:%M:%S %Y"))
    if abs(old_start-prior_process["started_unix"]) > 2 or process["started_unix"] <= old_start+2:
        raise ValueError("PID reuse lacks distinct old observed lifetime")
    if record["argv"][1:] != expected_args:
        raise ValueError("interrupted native argv differs from read-only request")
    relative = tail["record"]["path"]
    key = RUN.sha(RUN.encoded(record["key"]))
    if RUN.read_json(RUN.private_child(old, "steps/"+key+".json")) != {"record": relative, "sequence": record["sequence"]}:
        raise ValueError("interrupted tail step differs")
    # Retain failed streams as bytes only: even syntactically complete JSON is
    # never a successful page. Limits and exact hashes apply to partial bytes.
    for name in ["stdout", "stderr"]:
        stream = record[name]
        path = RUN.private_child(old, stream["path"])
        if (stream["path"] != f"commands/{record['sequence']:09d}/{name}"
                or path.stat().st_size != stream["bytes"] or stream["bytes"] > 8*RUN.MIB
                or RUN.file_sha(path) != stream["sha256"]):
            raise ValueError("interrupted tail raw stream changed")
    if (review.get("status") != "PASS" or review.get("failed_result_sha256") != evidence["result"]["sha256"]
            or review.get("phase_sha256") != evidence["phase"]["sha256"]
            or review.get("interrupted_tail_sha256") != RUN.sha(RUN.encoded(tail))
            or review.get("next_command") != request["expected_next_command"]
            or review.get("known_processes_absent") is not True or review.get("no_unresolved_command") is not True
            or review.get("terminal_failed_commands") != 1
            or review.get("terminal_success_commands") != request["expected_next_command"]-2):
        raise ValueError("PID repair independent ownership/tail review missing")
    if os.path.lexists(old/"pause-request"):
        raise ValueError("failed PID predecessor has a pause")
    if RUN.evidence_document(old, request["journal"]) != {"next_command": request["expected_next_command"]}:
        raise ValueError("failed PID predecessor journal changed")
    return {"failure_classification_retained": failed["status"], "supervisor_ownership_retained": failed["ownership_status"],
            "review": evidence["ownership_review"], "excluded_failed_tail": tail,
            "successful_active_pages": active["page_next"], "failed_page_index": active["page_next"],
            "scope": "verified separate PID lifetimes and terminal commands; precise new ps start not retained; no global absence claim"}


class PidRepairOrigins(RepairOrigins):
    """Exactly the reviewed v5 -> v4 -> v2 chain, with unambiguous names."""
    def __init__(self, runner, old, binding, request, prefix, verify_commands=True):
        self.descriptors = []
        self.roots = {"v5": {"root": str(old), "binding": binding, "prefix": prefix, "state": request["expected_source_state"]}}
        self.lineage_evidence = []
        try:
            config = RUN.read_json(old/"config.json")
            if RUN.sha(RUN.encoded(config)) != binding.get("config_sha256"):
                raise ValueError("PID repair predecessor config binding differs")
            parent_request = config["generation_request"]
            inherited = RUN.evidence_document(old, request["inherited_adoption"])
            if (inherited.get("protocol") != 2 or parent_request.get("protocol") != 2
                    or inherited.get("source_binding") != binding or inherited.get("after_logical_state_equal") is not True
                    or inherited.get("request_sha256") != RUN.sha(RUN.encoded(parent_request))
                    or inherited["predecessor"] != parent_request["predecessor"]
                    or inherited["predecessor_state"] != parent_request["expected_source_state"]
                    or RUN.sha(RUN.encoded(inherited["prefix"])) != parent_request["command_index_sha256"]):
                raise ValueError("PID repair protocol2 ancestry binding differs")
            v4 = RUN.no_links(inherited["predecessor"])
            if v4 == old:
                raise ValueError("PID repair lineage cycle")
            root, lock = old_lock(runner, {"predecessor": str(v4)}); self.descriptors.append(lock)
            v4_binding = RUN.evidence_document(root, parent_request["binding"])
            if v4_binding != inherited["predecessor_binding"]:
                raise ValueError("PID repair parent source binding differs")
            ancestor = RUN.evidence_document(root, parent_request["inherited_adoption"])
            if ancestor.get("protocol") != 1 or ancestor.get("source_binding") != v4_binding or ancestor.get("after_logical_state_equal") is not True:
                raise ValueError("PID repair protocol1 ancestry differs")
            v2 = RUN.no_links(ancestor["predecessor"])
            if v2 in {old, v4}:
                raise ValueError("PID repair ancestor cycle")
            _, lock = old_lock(runner, {"predecessor": str(v2)}); self.descriptors.append(lock)
            parent_roots = {"local": {"root": str(v4), "binding": v4_binding, "prefix": inherited["prefix"], "state": inherited["predecessor_state"]},
                            "inherited": {"root": str(v2), "binding": ancestor["predecessor_binding"], "prefix": ancestor["prefix"], "state": ancestor["predecessor_state"]}}
            if inherited["origins"] != parent_roots:
                raise ValueError("PID repair inherited origins differ")
            self.roots.update(v4=parent_roots["local"], v2=parent_roots["inherited"])
            self.lineage_evidence = [(old, request["inherited_adoption"]), (v4, parent_request["inherited_adoption"]),
                (old, {"path": "reports/generation-reconciled.json", "sha256": inherited["reconciled_sha256"]}),
                (v4, {"path": "reports/generation-before.json", "sha256": ancestor["before_sha256"]}),
                (v4, {"path": "reports/generation-after.json", "sha256": ancestor["after_sha256"]})]
            prior_scan = RUN.evidence_document(*self.lineage_evidence[2])
            before_scan = RUN.evidence_document(*self.lineage_evidence[3])
            after_scan = RUN.evidence_document(*self.lineage_evidence[4])
            if (prior_scan["plan_schema"] != 2 or after_scan["plan_schema"] != 2
                    or {k:v for k,v in before_scan.items() if k != "plan_schema"} != {k:v for k,v in after_scan.items() if k != "plan_schema"}
                    or prior_scan["retained_tables"] != parent_request["expected_table_counts"]
                    or prior_scan["rows_per_revision"] != parent_request["expected_rows"]
                    or set(prior_scan["tables"]) != set(TABLES)):
                raise ValueError("PID repair prior typed reconciliation differs")
            self.parent_failure = (v4, parent_request, v4_binding)
            failed_checkpoint(*self.parent_failure)
            total = sum(len(value["prefix"]) for value in self.roots.values())
            if total > request["limits"]["commands"]:
                raise ValueError("PID repair total ancestry command cap")
            for descriptor in self.roots.values():
                paths = set()
                for key, reference in descriptor["prefix"].items():
                    if reference["path"] in paths:
                        raise ValueError("duplicate PID repair origin reference")
                    paths.add(reference["path"])
                    if verify_commands:
                        record = checked_record(Path(descriptor["root"]), reference, descriptor["binding"])
                        if (key != RUN.sha(RUN.encoded(record["key"])) or record.get("exit_code") != 0
                                or record.get("failure") or record.get("log_errors")):
                            raise ValueError("PID repair inherited prefix mismatch")
            self.unchanged()
        except BaseException:
            self.close(); raise

    def reference(self, sequence):
        if type(sequence) is int and sequence > 0:
            origin, number = "v5", f"{sequence:09d}"
        elif isinstance(sequence, str):
            parts = sequence.split("/")
            if len(parts) != 3 or parts[0] != "adopted":
                raise ValueError("invalid PID repair qualified command")
            origin = {"local": "v4", "inherited": "v2"}.get(parts[1], parts[1])
            number = parts[2]
            if origin not in self.roots or len(number) != 9 or not number.isascii() or not number.isdigit():
                raise ValueError("invalid PID repair origin/sequence")
        else:
            raise ValueError("invalid PID repair command identity")
        matches = [value for value in self.roots[origin]["prefix"].values() if value["path"] == f"commands/{number}/result.json"]
        if len(matches) != 1:
            raise ValueError("missing/ambiguous PID repair command")
        return {"origin": origin, "reference": matches[0]}

    def unchanged(self):
        super().unchanged()
        for root, reference in self.lineage_evidence:
            RUN.evidence_document(root, reference)
        if hasattr(self, "parent_failure"):
            failed_checkpoint(*self.parent_failure)


class PidRepairReplay(PidRepairOrigins, RepairReplay):
    def __init__(self, runner):
        self.runner = runner; self.request = runner.config["generation_request"]
        self.receipt = RUN.read_json(runner.root/"reports/generation-adoption.json")
        if (self.receipt.get("protocol") != 3 or self.receipt["request_sha256"] != RUN.sha(RUN.encoded(self.request))
                or self.receipt["source_binding"] != runner.binding or self.receipt["after_logical_state_equal"] is not True
                or RUN.sha(RUN.encoded(self.receipt["prefix"])) != self.request["command_index_sha256"]):
            raise ValueError("PID repair replay receipt binding differs")
        self.old, lock = old_lock(runner, self.request)
        try:
            binding = RUN.evidence_document(self.old, self.request["binding"])
            super().__init__(runner, self.old, binding, self.request, self.receipt["prefix"], verify_commands=False)
            self.descriptors.append(lock)
            if (self.roots != self.receipt["origins"] or self.receipt["predecessor"] != str(self.old)
                    or self.receipt["predecessor_binding"] != binding
                    or self.receipt["predecessor_state"] != self.request["expected_source_state"]
                    or self.receipt["failure_proof"] != pid_failed_checkpoint(self.old, self.request, binding)):
                raise ValueError("PID repair replay origin/failure binding differs")
        except BaseException:
            if lock not in getattr(self, "descriptors", []): os.close(lock)
            self.close(); raise

    def replay(self, key, arguments):
        reference = self.receipt["prefix"].get(RUN.sha(RUN.encoded(key)))
        if reference is None:
            return None  # The explicitly excluded interrupted tail executes anew only in this new generation.
        reference = {"origin": "v5", "reference": reference}
        root, record = self.load(reference)
        expected = [str(self.runner.root/"plans/main") if item == str(root/"plans/main") else item for item in record["requested_arguments"]]
        if record["key"] != key or expected != [str(item) for item in arguments]:
            raise ValueError("PID repair replay arguments differ")
        return self.result(reference)
