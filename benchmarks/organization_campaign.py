#!/usr/bin/env python3
"""Serial sc-22842 production-query campaign; exclusive private outputs only.

Preparation is synthetic normalized data. This driver makes no image-import,
RAW, preview-delivery, or complete-story acceptance claim.
"""
from __future__ import annotations
import argparse
import hashlib
import json
import math
import pathlib
import shutil
import subprocess
import time
import signal
import datetime
import psutil
import os
import stat

PROTOCOL = 1  # Frozen fixture/native row protocol.
DRIVER_PROTOCOL = 3
TEXT_LIMITS = {"document_bytes": 1024**2, "page_bytes": 8 * 1024**2}
LOCAL_TEXT_CASES = {"filename-reverse", "mixed", "text-capture"}
DIAGNOSTIC_CASES = ["filename-reverse", "text", "text-capture", "date-camera"]
DIAGNOSTIC_SCALES = [1_000_000, 10_000_000]
SCALES = [1_000_000, 5_000_000, 10_000_000]
CASES = ["browse", "rating", "rating-sort", "capture-rating", "filename-reverse", "keyword",
         "wide-keyword", "collection", "mixed", "text", "text-capture", "camera-lens",
         "date-camera", "label-flag", "folder", "folder-recursive", "conflicted"]
SETTINGS = {"cache_size": -262144, "foreign_keys": 1, "journal_mode": "wal",
            "mmap_size": 0, "synchronous": 2, "temp_store": 1}


def save(path, value):
    with pathlib.Path(path).open("x", encoding="utf-8") as stream:
        json.dump(value, stream, indent=2, allow_nan=False)
        stream.write("\n")
        stream.flush()
        import os
        os.fsync(stream.fileno())


def sha(path):
    digest = hashlib.sha256()
    with pathlib.Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(4 * 1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def finite(value):
    return type(value) in (int, float) and math.isfinite(value) and value >= 0


def distribution(values):
    if not values or not all(finite(v) for v in values):
        raise ValueError("missing/nonfinite/negative timing")
    ordered = sorted(values)
    def q(fraction):
        position = (len(ordered) - 1) * fraction
        lo, hi = math.floor(position), math.ceil(position)
        return ordered[lo] + (ordered[hi] - ordered[lo]) * (position - lo)
    return {"n": len(values), "p50": q(.5), "p95": q(.95), "p99": q(.99), "max": max(values)}


def run_child(binary, catalog, output, args, timeout):
    """Always terminate/reap a child on observation failure; preserve raw stderr."""
    output = pathlib.Path(output)
    began = time.monotonic()
    begin_monotonic_ns=time.monotonic_ns()
    begin_utc=datetime.datetime.now(datetime.timezone.utc).isoformat()
    child = None
    rss = []
    error = None
    with output.with_suffix(".stderr").open("xb") as stderr:
        try:
            child = subprocess.Popen([str(binary), "--catalog", str(catalog), "--output", str(output), *args],
                                     stdout=subprocess.DEVNULL, stderr=stderr)
            process = psutil.Process(child.pid)
            while child.poll() is None:
                try:
                    rss.append(process.memory_info().rss)
                except psutil.NoSuchProcess:
                    break
                if time.monotonic() - began > timeout:
                    raise TimeoutError("declared child deadline exceeded")
                time.sleep(.02)
            child.wait(timeout=5)
        except Exception as exc:
            error = f"{type(exc).__name__}: {exc}"
        finally:
            if child is not None and child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=5)
    observer = {"exit_code": None if child is None else child.returncode,
                "elapsed_ms": (time.monotonic() - began) * 1000,
                "rss_peak_bytes": max(rss) if rss else None, "rss_samples": len(rss), "error": error,
                "begin_utc":begin_utc,"end_utc":datetime.datetime.now(datetime.timezone.utc).isoformat(),
                "begin_monotonic_ns":begin_monotonic_ns,"end_monotonic_ns":time.monotonic_ns()}
    save(output.with_suffix(".observer.json"), observer)
    return observer


def row_valid(row):
    i = row["sequence"]
    return (type(i) is int and i > 0 and row["asset_id"] == f"fixture-{i:012}"
            and row["state"] == "ready" and row["metadata_revision"] == 0
            and row["folder"] == 2 + i % 5 and row["filename"] == f"file{i:012}.jpg"
            and row["capture"] == f"2024-01-{i % 28 + 1:02}T12:00:00"
            and row["camera_make"] == "fixture" and row["camera"] == f"camera{i % 3}"
            and row["lens"] == f"lens{i % 4}" and row["format"] == ("JPEG" if i % 2 == 0 else "DNG")
            and row["rating"] == i % 6 and row["flag"] == ["reject", "pick", "unflagged"][i % 3]
            and row["label"] == ("red" if i % 2 == 0 else "blue")
            and row["conflicts"] == (["gps_latitude"] if i % 97 == 0 else [])
            and row["provenance"] == {"synthetic_fixture": PROTOCOL})



def expected_sequences(case, count, anchor):
    predicate = {
            "browse": lambda i: True, "folder-recursive": lambda i: True, "rating-sort": lambda i: True,
            "rating": lambda i: i % 6 == 4, "capture-rating": lambda i: i % 6 == 4,
            "filename-reverse": lambda i: i % 5 == 0, "text": lambda i: i % 5 == 0,
            "keyword": lambda i: i % 7 == 1, "wide-keyword": lambda i: i % 10000 == 1000,
            "collection": lambda i: i % 11 == 3,
            "mixed": lambda i: i % 420 == 400 and 4 <= i % 28 < 19,
            "text-capture": lambda i: i % 20 == 0,
            "camera-lens": lambda i: i % 12 == 4,
            "date-camera": lambda i: i % 3 == 1 and 4 <= i % 28 < 19,
            "label-flag": lambda i: i % 6 == 4,
            "folder": lambda i: i % 5 == 1,
            "conflicted": lambda i: i % 97 == 0,
        }[case]
    sequence = anchor["sequence"]
    if case in ("capture-rating", "text-capture", "date-camera"):
        day = int(anchor["key"]["value"][8:10])
        def ordered():
            for current_day in range(day, 29):
                lower = sequence + 1 if current_day == day else 1
                first = lower + (current_day - 1 - lower % 28) % 28
                yield from range(first, count + 1, 28)
        candidates = ordered()
    elif case == "rating-sort":
        stars=anchor["key"]["value"]
        def ordered():
            for rating in range(stars,6):
                lower=sequence+1 if rating==stars else 1
                first=lower+(rating-lower%6)%6
                yield from range(first,count+1,6)
        candidates=ordered()
    elif case == "filename-reverse":
        candidates = range(sequence - 1, 0, -1)
    else:
        candidates = range(sequence + 1, count + 1)
    result = []
    for i in candidates:
        if predicate(i):
            result.append(i)
            if len(result) == 200:
                break
    return result

def validate_text_work(chunk, local_text):
    work = chunk["text_work"]
    assert set(work) == {"candidate_rows_read", "indexed_rows", "indexed_bytes", "batches", "vm_steps", "sorts", "admission_limited"}
    for name in ("candidate_rows_read", "indexed_rows", "indexed_bytes", "batches", "vm_steps", "sorts"):
        assert type(work[name]) is int and work[name] >= 0
    # Frozen fixture documents are 11 or 14 UTF-8 bytes. Even 4096 candidates
    # cannot reach the default byte limit; unexpected admission is a failure.
    assert work["admission_limited"] is False
    assert work["candidate_rows_read"] == chunk["scanned"]
    assert work["sorts"] == 0 and chunk["vm_steps"] >= work["vm_steps"]
    if local_text:
        assert work["vm_steps"] > 0
        assert 0 <= work["indexed_rows"] <= chunk["scanned"]
        assert work["indexed_rows"] >= chunk["returned"]
        assert work["batches"] <= work["indexed_rows"] <= 128 * work["batches"]
        assert 11 * work["indexed_rows"] <= work["indexed_bytes"] <= 14 * work["indexed_rows"]
        assert work["indexed_bytes"] <= TEXT_LIMITS["page_bytes"]
    else:
        assert all(work[k] == 0 for k in ("indexed_rows", "indexed_bytes", "batches", "vm_steps", "sorts"))


def validate_receipt(receipt, observer, case, count, repetitions, warmups, start):
    """Fail closed on errors, truncated workloads, missing memory, or false pages."""
    assert observer["exit_code"] == 0 and observer["error"] is None
    assert type(observer["rss_samples"]) is int and observer["rss_samples"] > 0
    assert finite(observer["rss_peak_bytes"]) and observer["rss_peak_bytes"] > 0
    assert receipt["protocol"] == PROTOCOL and receipt["complete"] is True
    assert receipt["mode"] == "query" and receipt["count"] == count and receipt["case"] == case.replace("-", "_")
    assert receipt["repetitions"] == repetitions and receipt["warmups"] == warmups and receipt["start"] == start
    assert receipt["errors"] == [] and receipt["plans"] and all(isinstance(p, str) for p in receipt["plans"])
    assert receipt["settings"]["diagnostic_shared_helper_connection"] == SETTINGS
    assert receipt["engine_version"] == "3.51.1"
    assert receipt["text_limits"] == TEXT_LIMITS
    virtual_plans = [p for p in receipt["plans"] if "VIRTUAL TABLE" in p]
    assert len(virtual_plans) == (1 if case == "text" else 0)
    assert finite(receipt["open_ms"])
    assert len(receipt["samples"]) == repetitions and len(receipt["warmup_samples"]) == warmups
    for index, sample in enumerate(receipt["samples"]):
        assert sample["iteration"] == start + index
    assert all(sample["iteration"] == start for sample in receipt["warmup_samples"])
    for sample in receipt["warmup_samples"] + receipt["samples"]:
        assert sample["error"] is None and finite(sample["elapsed_ms"])
        rows, expected, chunks = sample["rows"], sample["oracle_sequences"], sample["chunks"]
        anchor = sample["anchor"]
        assert anchor["version"] == 1 and anchor["epoch"] == 0 and anchor["high_water"] == count
        assert anchor["sequence"] == count // 2 + count * 45 * (sample["iteration"] % 10) // 1000
        if case in ("capture-rating", "text-capture", "date-camera"):
            day = ([14, 18] if case == "date-camera" else [15, 26])[sample["iteration"] % 2]
            expected_key = {"kind": "text", "value": f"2024-01-{day:02}T12:00:00"}
        elif case == "rating-sort":
            expected_key={"kind":"integer","value":[3,5][sample["iteration"]%2]}
        elif case == "filename-reverse":
            expected_key = {"kind": "text", "value": f"file{anchor['sequence']:012}.jpg"}
        else:
            expected_key = {"kind": "integer", "value": anchor["sequence"]}
        assert anchor["key"] == expected_key
        assert chunks and [r["sequence"] for r in rows] == expected == expected_sequences(case, count, anchor)
        assert len(expected) == len(set(expected))
        assert all(row_valid(row) for row in rows)
        assert len(rows) <= 200
        # The independent arithmetic oracle is authoritative for every case.
        # In particular, text-capture's day-26/lens0 anchors have genuine empty
        # tails at every scale; they still owe full exhaustion and latency gates.
        if len(rows) < 200:
            assert chunks[-1]["exhausted"] is True
        assert sum(c["returned"] for c in chunks) == len(rows)
        assert sum(c["elapsed_ms"] for c in chunks) <= sample["elapsed_ms"] + .001
        for chunk in chunks:
            assert type(chunk["scanned"]) is int and 0 <= chunk["returned"] <= chunk["scanned"] <= 4096
            assert finite(chunk["vm_steps"]) and chunk["sorts"] == 0 and finite(chunk["elapsed_ms"])
            validate_text_work(chunk, case in LOCAL_TEXT_CASES)
            if case == "filename-reverse":
                assert chunk["text_work"]["indexed_rows"] == chunk["scanned"]
            if not chunk["exhausted"]:
                assert chunk["cursor"] is not None and chunk["has_more"] is True
    return distribution([s["elapsed_ms"] for s in receipt["samples"]])


def validate_transitions(data, observer, count, repetitions):
    assert observer["exit_code"] == 0 and observer["error"] is None
    assert observer["rss_samples"] > 0 and finite(observer["rss_peak_bytes"])
    assert data["protocol"] == 1 and data["mode"] == "transitions" and data["complete"] is True
    assert data["errors"] == [] and data["count"] == count and data["repetitions"] == repetitions
    assert data["engine_version"] == "3.51.1" and data["settings"]["diagnostic_shared_helper_connection"] == SETTINGS
    assert data["source_hashes_before"] == data["source_hashes_after"] and len(data["source_hashes_before"]) == repetitions
    assert all(isinstance(v,str) and len(v)==64 for v in data["source_hashes_before"])
    assert len(data["writes"]) == len(data["snapshot_browse"]) == len(data["source_updates"]) == repetitions
    for i, (write, browse, source) in enumerate(zip(data["writes"], data["snapshot_browse"], data["source_updates"])):
        assert all("error" not in row and finite(row["elapsed_ms"]) and row["iteration"]==i
                   and finite(row["begin_ms"]) and finite(row["end_ms"]) and row["end_ms"]>=row["begin_ms"] for row in (write,browse,source))
        assert write["asset_id"] == f"fixture-{count+1+i%100:012}"
        assert write["operation"] == ({"operation":"rating","value":i%6} if i%2==0 else {"operation":"label","value":f"saved{i}"})
        assert write["revision_before"]==i//100 and write["revision_after"]==i//100+1 and write["pixel_generation"]==0
        assert source["asset_id"]==f"fixture-{count+101+i%100:012}" and source["selected_label"]==f"import{i}"
        assert source["revision_before"]==i//100 and source["revision_after"]==i//100+1 and source["models"]
        page=browse["page"]
        assert page["sorts"]==0 and page["scanned"]==200 and page["page_complete"] is True
        validate_text_work(page, False)
        assert [row["sequence"] for row in page["rows"]]==list(range(i*200+1,(i+1)*200+1))
        assert all(row_valid(row) for row in page["rows"])
    assert len(data["reopened"])==min(repetitions,100)
    for item,i in zip(data["reopened"],range(max(0,repetitions-100),repetitions)):
        assert item["correct"] is True and item["asset_id"]==f"fixture-{count+1+i%100:012}" and item["revision"]==i//100+1
        assert (item["field"],item["value"])==(("rating",str(i%6)) if i%2==0 else ("label",f"saved{i}"))
    measurements={
        "rating":distribution([w["elapsed_ms"] for w in data["writes"] if w["operation"]["operation"]=="rating"]),
        "label":distribution([w["elapsed_ms"] for w in data["writes"] if w["operation"]["operation"]=="label"]),
        "snapshot_browse":distribution([w["elapsed_ms"] for w in data["snapshot_browse"]]),
        "source_update":distribution([w["elapsed_ms"] for w in data["source_updates"]]),
    }
    overlap={}
    for name,samples in [("writes",data["writes"]),("snapshot_browse",data["snapshot_browse"])]:
        yes=[];no=[]
        for sample in samples:
            overlapping=any(sample["begin_ms"]<source["end_ms"] and source["begin_ms"]<sample["end_ms"] for source in data["source_updates"])
            (yes if overlapping else no).append(sample["elapsed_ms"])
        overlap[name]={"overlapping_n":len(yes),"nonoverlapping_n":len(no),"overlapping":distribution(yes) if yes else None,"nonoverlapping":distribution(no) if no else None}
    measurements["overlap_diagnostic"]=overlap
    return measurements


def run_transitions(args, manifest):
    read_result=json.loads((args.root/"measurement"/"campaign.json").read_text())
    assert read_result["complete"] is True, "finish terminal read workloads before mutation copies"
    destination=args.root/"transitions";destination.mkdir()
    result={"protocol":1,"complete":False,"smoke":args.smoke,"scales":[],"errors":[]}
    for fixture in manifest["fixtures"]:
        count=fixture["count"];source=pathlib.Path(fixture["catalog"])/"catalog.sqlite3"
        directory=destination/str(count);directory.mkdir();catalog=directory/"catalog";catalog.mkdir()
        try:
            assert shutil.disk_usage(destination).free >= fixture["main_bytes"] + 1024**3
            before=sha(source);assert before==fixture["main_sha256"]
            sidecars={str(p):p.stat().st_size for p in source.parent.glob(source.name+"-*")}
            assert all(size==0 for path,size in sidecars.items() if path.endswith(("-wal","-journal"))), "uncheckpointed source"
            shutil.copy2(source,catalog/"catalog.sqlite3")
            copy_hash=sha(catalog/"catalog.sqlite3");after=sha(source)
            assert before==copy_hash==after
            assert sidecars=={str(p):p.stat().st_size for p in source.parent.glob(source.name+"-*")}
            proof={"source_before":before,"copy_before":copy_hash,"source_after_copy":after,"source_sidecars":sidecars}
            save(directory/"copy-proof.json",proof)
            repetitions=2 if args.smoke else 200
            receipt=directory/"native.json"
            observer=run_child(args.binary,catalog,receipt,["transitions","--repetitions",str(repetitions)],1800)
            data=json.loads(receipt.read_text())
            measures=validate_transitions(data,observer,count,repetitions)
            after_work=sha(source);assert after_work==before
            result["scales"].append({"count":count,"measurements":measures,"observer":observer,"copy_proof":proof,"source_after_work":after_work,
                                    "numerical_pass":all(measures[name]["p95"]<100 for name in ("rating","label","snapshot_browse")),"raw":str(receipt)})
        except Exception as error:
            result["errors"].append({"count":count,"error":f"{type(error).__name__}: {error}"})
    result["complete"]=len(result["scales"])==len(manifest["fixtures"]) and not result["errors"]
    result["transition_numerical_pass"]=result["complete"] and all(row["numerical_pass"] for row in result["scales"])
    result["limitations"]="Metadata-only source refresh on disposable copies; no RAW/image-import throughput claim. Mixed whole-process RSS reported diagnostically, browse-only memory is gated separately."
    save(destination/"campaign.json",result)
    if not result["transition_numerical_pass"]:
        raise SystemExit("transition gates failed; evidence retained")


def source_state(source):
    """Filesystem-only evidence: never open the preserved source with SQLite."""
    def info(path):
        st = path.lstat()
        assert stat.S_ISREG(st.st_mode), "source/companion must be a regular file"
        return {"size": st.st_size, "device": st.st_dev, "inode": st.st_ino,
                "mtime_ns": st.st_mtime_ns, "ctime_ns": st.st_ctime_ns}
    companions = {}
    for path in source.parent.glob(source.name + "-*"):
        assert path.name in {source.name + suffix for suffix in ("-wal", "-shm", "-journal")}, "unexpected source companion"
        item = info(path)
        if path.name.endswith(("-wal", "-journal")):
            assert item["size"] == 0, "uncheckpointed source"
        else:
            assert item["size"] <= 64 * 1024, "unexpected SHM size"
        item["sha256"] = sha(path)
        companions[path.name] = item
    return {"main": info(source), "companions": companions}


def copied_fixture(args, old_manifest_path, fixture):
    source = pathlib.Path(fixture["catalog"]) / "catalog.sqlite3"
    for preserved in (old_manifest_path.parent.resolve(), source.parent.resolve()):
        target = args.root.resolve()
        assert target != preserved and not target.is_relative_to(preserved) and not preserved.is_relative_to(target), "reuse destination overlaps preserved evidence"
    before = source_state(source)
    assert before["main"]["size"] == fixture["main_bytes"]
    assert shutil.disk_usage(args.root).free >= fixture["main_bytes"] + 1024**3
    with source.open("rb") as stream:
        header = stream.read(100)
    assert header[:16] == b"SQLite format 3\0"
    source_schema = int.from_bytes(header[60:64], "big")
    assert source_schema in (4, 5), "source schema is not 4 or 5"
    assert int.from_bytes(header[68:72], "big") == 0x50484341, "source is not PhotoCatalog"
    before_hash = sha(source)
    assert before_hash == fixture["main_sha256"], "source main changed"
    prepare = old_manifest_path.parent / f"prepare-{fixture['count']}.json"
    assert sha(prepare) == fixture["prepare_receipt_sha256"]
    data = json.loads(prepare.read_text())
    assert data["protocol"] == PROTOCOL and data["complete"] is True and data["mode"] == "prepare" and data["count"] == fixture["count"]
    expected_counts = {name: fixture["count"] * multiplier for name, multiplier in
                       (("assets",1),("organization_assets",1),("organization_keyword_members",4),("organization_folder_members",2),("organization_text",1))}
    assert len(data["counts"]) == 5 and dict(data["counts"]) == expected_counts
    assert data["engine_version"] == "3.51.1" and data["settings"]["diagnostic_shared_helper_connection"] == SETTINGS
    catalog = args.root / f"catalog-{fixture['count']}"
    catalog.mkdir()
    target = catalog / "catalog.sqlite3"
    # Exclusive output, ordinary complete copy; no source SQLite connection,
    # checkpoint, journal deletion, or source permission change is allowed.
    with source.open("rb") as incoming, target.open("xb") as outgoing:
        shutil.copyfileobj(incoming, outgoing, 4 * 1024**2)
        outgoing.flush()
        os.fsync(outgoing.fileno())
    copy_hash, after_hash = sha(target), sha(source)
    after = source_state(source)
    assert copy_hash == before_hash == after_hash and before == after, "source changed while copying"
    # Preserve exact original receipt bytes, not a new serialization identity.
    with (args.root / prepare.name).open("xb") as output:
        output.write(prepare.read_bytes())
        output.flush(); os.fsync(output.fileno())
    assert sha(args.root / prepare.name) == fixture["prepare_receipt_sha256"]
    proof = {"source": str(source.resolve()), "source_before": before, "source_after": after,
             "source_before_sha256": before_hash, "source_after_sha256": after_hash,
             "copy_sha256": copy_hash, "schema": source_schema, "application_id": 0x50484341,
             "prepare_receipt_sha256": fixture["prepare_receipt_sha256"]}
    save(args.root / f"reuse-{fixture['count']}.json", proof)
    return {**fixture, "catalog": str(catalog.resolve()), "reuse_proof": str(args.root / f"reuse-{fixture['count']}.json")}


def migrate_reused_fixture(args, fixture):
    catalog = pathlib.Path(fixture["catalog"])
    main = catalog / "catalog.sqlite3"
    before = sha(main)
    assert before == fixture["main_sha256"]
    receipt = args.root / f"migrate-{fixture['count']}.json"
    observer = run_child(args.binary, catalog, receipt, ["migrate-fixture"], 4 * 3600)
    native = json.loads(receipt.read_text())
    assert observer["exit_code"] == 0 and observer["error"] is None
    assert native["protocol"] == PROTOCOL and native["complete"] is True and native["mode"] == "migrate_fixture"
    assert native["count"] == fixture["count"] and native["schema_before"] in (4,5) and native["schema_after"] == 5
    assert native["engine_version"] == "3.51.1"
    assert native["logical_before"] == native["logical_after"] and len(native["logical_before"]) == 64
    assert native["table_counts_before"] == native["table_counts_after"] and native["table_counts_before"]
    assert native["index_sql"] == "CREATE INDEX organization_lens_capture ON organization_assets(lens,capture,sequence)"
    after = sha(main)
    proof = {"owned_copy_before_sha256":before,"owned_copy_after_sha256":after,
             "schema_before":native["schema_before"],"schema_after":5,"native_receipt":str(receipt),
             "native_receipt_sha256":sha(receipt),"observer":observer,
             "logical_before":native["logical_before"],"logical_after":native["logical_after"],
             "table_counts_before":native["table_counts_before"],"table_counts_after":native["table_counts_after"],
             "index_sql":native["index_sql"]}
    proof_path=args.root/f"migration-proof-{fixture['count']}.json"
    save(proof_path,proof)
    return {**fixture,"source_main_sha256":before,"main_sha256":after,"main_bytes":main.stat().st_size,
            "schema":5,"migration_proof":str(proof_path)}


def prepare_reuse(args, counts):
    old_path = args.reuse_prepared.resolve()
    before = sha(old_path)
    old = json.loads(old_path.read_text())
    assert old["protocol"] == PROTOCOL and old["complete"] is True and old["scales"] == counts and old["smoke"] == args.smoke
    assert [f["count"] for f in old["fixtures"]] == counts
    result = {"protocol": PROTOCOL, "driver_protocol": DRIVER_PROTOCOL, "complete": False,
              "smoke": args.smoke, "scales": counts, "fixtures": [],
              "reuse_manifest": str(old_path), "reuse_manifest_sha256": before}
    try:
        for fixture in old["fixtures"]:
            result["fixtures"].append(migrate_reused_fixture(args, copied_fixture(args, old_path, fixture)))
        assert sha(old_path) == before, "preserved manifest changed"
        result["complete"] = True
    except Exception as error:
        result["error"] = f"{type(error).__name__}: {error}"
    save(args.root / "manifest.json", result)
    if not result["complete"]:
        raise SystemExit("reuse preparation failed; all evidence retained")


def run_diagnostic(args, manifest):
    destination = args.root / "diagnostic"
    destination.mkdir()
    scales = [1000] if args.smoke else DIAGNOSTIC_SCALES
    result = {"driver_protocol": DRIVER_PROTOCOL, "complete": False, "acceptance_evidence": False,
              "cases": [], "errors": [], "source_proofs": [], "samples": 5, "warmups": 3,
              "deadline_seconds": 120, "scales": scales, "case_names": DIAGNOSTIC_CASES}
    for fixture in manifest["fixtures"]:
        count = fixture["count"]
        if count not in scales: continue
        catalog = pathlib.Path(fixture["catalog"])
        before = None
        try:
            before = sha(catalog / "catalog.sqlite3")
            assert before == fixture["main_sha256"]
            for case in DIAGNOSTIC_CASES:
                output = destination / f"{count}-{case}.json"
                observer = run_child(args.binary, catalog, output,
                                     ["query", case, "--repetitions", "5", "--warmups", "3", "--start", "0"], 120)
                try:
                    receipt = json.loads(output.read_text())
                    timings = validate_receipt(receipt, observer, case, count, 5, 3, 0)
                    result["cases"].append({"count": count, "case": case, "distribution": timings,
                                            "observer": observer, "raw": str(output)})
                except Exception as error:
                    result["errors"].append({"count": count, "case": case, "error": f"{type(error).__name__}: {error}", "observer": observer, "raw": str(output)})
        except Exception as error:
            result["errors"].append({"count": count, "error": f"{type(error).__name__}: {error}"})
        try:
            after = sha(catalog / "catalog.sqlite3")
            result["source_proofs"].append({"count": count, "before": before, "after": after, "unchanged": before == after})
            assert before == after, "diagnostic changed fixture"
        except Exception as error:
            result["errors"].append({"count": count, "error": f"{type(error).__name__}: {error}"})
    result["complete"] = not result["errors"] and len(result["cases"]) == len(scales)*len(DIAGNOSTIC_CASES)
    result["review_required"] = "Inspect all latency/counter/RSS outcomes before starting the full campaign. These five samples are diagnostic, not qualifying tails."
    save(destination / "diagnostic.json", result)
    if not result["complete"]:
        raise SystemExit("bounded diagnostic failed; retained all outcomes")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=pathlib.Path, required=True)
    parser.add_argument("--root", type=pathlib.Path, required=True)
    parser.add_argument("--build-reference", type=pathlib.Path, required=True,
                        help="Root-owned build receipt with binary_sha256 and source_commit")
    parser.add_argument("--phase", choices=["prepare", "diagnostic", "measure", "transitions"], required=True)
    parser.add_argument("--smoke", action="store_true", help="1000-row functional run, never scale acceptance")
    parser.add_argument("--reuse-prepared", type=pathlib.Path, help="Prepare only: copy an explicitly supplied protocol-1 pristine manifest into a new root")
    args = parser.parse_args()
    assert args.reuse_prepared is None or args.phase == "prepare", "reuse is a preparation operation"
    if not __debug__:
        raise RuntimeError("Python optimization disables evidence assertions; run without -O")
    build = json.loads(args.build_reference.read_text())
    assert build["binary_sha256"] == sha(args.binary)
    assert isinstance(build["source_commit"], str) and len(build["source_commit"]) == 40 and all(c in "0123456789abcdef" for c in build["source_commit"])
    assert build["profile"] == ("debug" if args.smoke else "release") and build["command"] and build["build_log_sha256"]
    if not args.smoke:
        assert build["working_tree_dirty"] is False
    counts = [1000] if args.smoke else SCALES
    if args.phase == "prepare":
        if args.reuse_prepared is not None:
            old = json.loads(args.reuse_prepared.read_text())
            target = args.root.resolve()
            preserved_roots = [args.reuse_prepared.resolve().parent] + [pathlib.Path(f["catalog"]).resolve() for f in old["fixtures"]]
            for preserved in preserved_roots:
                assert target != preserved and not target.is_relative_to(preserved) and not preserved.is_relative_to(target), "new root overlaps preserved campaign/catalog"
        args.root.mkdir()  # Refuse an existing root, including partial/failed runs.
        save(args.root / "build-reference.json", build)
        save(args.root / "driver-source.json", {"sha256": sha(__file__), "source": pathlib.Path(__file__).read_text()})
        if args.reuse_prepared is not None:
            prepare_reuse(args, counts)
            return
        manifest = {"protocol": PROTOCOL, "driver_protocol": DRIVER_PROTOCOL, "complete": False, "smoke": args.smoke, "scales": counts, "fixtures": []}
        try:
            for count in counts:
                # Conservative explicit admission, not an estimate of actual usage.
                assert shutil.disk_usage(args.root).free >= count * 8192 + 1024 ** 3, "insufficient preparation free space"
                catalog = args.root / f"catalog-{count}"
                receipt = args.root / f"prepare-{count}.json"
                observer = run_child(args.binary, catalog, receipt, ["prepare", "--count", str(count)], 4 * 3600)
                data = json.loads(receipt.read_text())
                assert observer["exit_code"] == 0 and observer["error"] is None and data["complete"] is True
                assert data["count"] == count and data["mode"] == "prepare"
                manifest["fixtures"].append({"count": count, "catalog": str(catalog.resolve()),
                                             "main_sha256": sha(catalog / "catalog.sqlite3"), "prepare_receipt_sha256": sha(receipt),
                                             "main_bytes": (catalog / "catalog.sqlite3").stat().st_size})
            manifest["complete"] = True
        except Exception as error:
            manifest["error"] = f"{type(error).__name__}: {error}"
        save(args.root / "manifest.json", manifest)
        if not manifest["complete"]:
            raise SystemExit("preparation failed; retained manifest")
        return
    manifest = json.loads((args.root / "manifest.json").read_text())
    assert manifest["complete"] and manifest["scales"] == counts and manifest["smoke"] == args.smoke
    assert json.loads((args.root / "build-reference.json").read_text()) == build
    assert json.loads((args.root / "driver-source.json").read_text())["sha256"] == sha(__file__), "driver changed since preparation"
    assert manifest["driver_protocol"] == DRIVER_PROTOCOL
    assert [f["count"] for f in manifest["fixtures"]] == counts
    if args.phase == "diagnostic":
        run_diagnostic(args, manifest)
        return
    if args.phase == "transitions":
        run_transitions(args, manifest)
        return
    if not args.smoke:
        diagnostic = json.loads((args.root / "diagnostic" / "diagnostic.json").read_text())
        assert diagnostic["complete"] is True and diagnostic["driver_protocol"] == DRIVER_PROTOCOL and diagnostic["acceptance_evidence"] is False
        assert diagnostic["scales"] == DIAGNOSTIC_SCALES and diagnostic["case_names"] == DIAGNOSTIC_CASES
    destination = args.root / "measurement"
    destination.mkdir()
    result = {"protocol": PROTOCOL, "driver_protocol": DRIVER_PROTOCOL, "complete": False, "smoke": args.smoke, "cases": [], "source_proofs": [], "errors": []}
    for fixture in manifest["fixtures"]:
        count, catalog = fixture["count"], pathlib.Path(fixture["catalog"])
        try:
            assert sha(catalog / "catalog.sqlite3") == fixture["main_sha256"], "pristine main changed"
            for case in CASES:
                record = {"count": count, "case": case, "errors": [], "trials": []}
                for kind, index, repetitions, warmups in [("warm", 0, 2 if args.smoke else 100, 1 if args.smoke else 3)] + [("fresh", i, 1, 0) for i in range(2 if args.smoke else 20)]:
                    receipt = destination / f"{count}-{case}-{kind}-{index}.json"
                    observer = run_child(args.binary, catalog, receipt, ["query", case, "--repetitions", str(repetitions), "--warmups", str(warmups), "--start", str(index)], 1800)
                    try:
                        data = json.loads(receipt.read_text())
                        timings = validate_receipt(data, observer, case, count, repetitions, warmups, index)
                        record["trials"].append({"kind": kind, "index": index, "distribution": timings, "observer": observer, "raw": str(receipt), "elapsed_samples_ms": [s["elapsed_ms"] for s in data["samples"]]})
                    except Exception as error:
                        record["errors"].append({"kind": kind, "index": index, "error": f"{type(error).__name__}: {error}", "observer": observer, "raw": str(receipt)})
                good = record["trials"]
                record["numerical_pass"] = (not record["errors"] and len(good) == (3 if args.smoke else 21)
                    and all(t["observer"]["rss_peak_bytes"] <= 4 * 1024**3 for t in good)
                    and all(t["distribution"]["p95"] < 100 for t in good if t["kind"] == "warm")
                    and all(t["observer"]["elapsed_ms"] < 1000 for t in good if t["kind"] == "fresh"))
                result["cases"].append(record)
            after = sha(catalog / "catalog.sqlite3")
            proof = {"count": count, "before": fixture["main_sha256"], "after": after, "unchanged": after == fixture["main_sha256"],
                     "companions": {p.name: p.stat().st_size for p in catalog.glob("catalog.sqlite3-*")}}
            result["source_proofs"].append(proof)
            assert proof["unchanged"], "query campaign changed fixture main"
        except Exception as error:
            result["errors"].append({"count": count, "error": f"{type(error).__name__}: {error}"})
    result["complete"] = len(result["cases"]) == len(counts)*len(CASES) and len(result["source_proofs"]) == len(counts) and not result["errors"]
    result["query_numerical_pass"] = result["complete"] and all(c["numerical_pass"] for c in result["cases"])
    result["scale_numerical_evidence"] = result["query_numerical_pass"] and not args.smoke
    result["limitations"] = "Numerical query gates only. Bounded engine-work interpretation, source transition/mixed workload, hardware observer, integration CI and independent review remain required."
    save(destination / "campaign.json", result)
    if not result["query_numerical_pass"]:
        raise SystemExit("query gates failed; raw samples retained")


if __name__ == "__main__":
    def stop(signum, frame):
        raise KeyboardInterrupt(f"received signal {signum}")
    signal.signal(signal.SIGTERM, stop)
    main()
