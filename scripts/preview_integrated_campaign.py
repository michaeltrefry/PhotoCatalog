#!/usr/bin/env python3
"""Fixed 10M retained-page experiment on a raw copied schema-4 fixture only.

Never opens a source database through SQLite. Both commands require a reviewed
binding and an explicit execution lane; copying and hashing are untimed setup.
"""
from __future__ import annotations
import argparse
import os
from pathlib import Path
import shutil
import subprocess

from preview_experiment import digest, exclusive
from preview_host import HostObservation, host_identity
from preview_navigation_campaign import anchor, read_json, expected_trials, validate_trial, distribution

SUFFIXES = ("", "-wal", "-shm", "-journal")


def source_state(catalog):
    state = {}
    for suffix in SUFFIXES:
        path = Path(str(catalog) + suffix)
        if path.is_symlink():
            raise ValueError("source companions must be regular files, not symlinks")
        if not path.exists():
            state[suffix] = {"present": False}
        else:
            if not path.is_file():
                raise ValueError("source companion is not a file")
            state[suffix] = {"present": True, "bytes": path.stat().st_size, "sha256": digest(path)}
    if not state[""]["present"]:
        raise ValueError("source main missing")
    return state


def raw_copy(source, target, expected, after_copy=None):
    """Exclusive byte copy, no database recovery or connection to the source."""
    before = source_state(source)
    if before != expected:
        raise ValueError("source does not match frozen main/companion identities")
    target.parent.mkdir(parents=False, exist_ok=False)
    for suffix in SUFFIXES:
        if before[suffix]["present"]:
            src, dst = Path(str(source)+suffix), Path(str(target)+suffix)
            with src.open("rb") as inp, dst.open("xb") as out:
                shutil.copyfileobj(inp, out, length=1024*1024)
                out.flush()
                os.fsync(out.fileno())
    if after_copy is not None:  # Test-only fault injection; CLI never provides this.
        after_copy()
    copied, after = source_state(target), source_state(source)
    if copied != before or after != before:
        raise ValueError("source changed during raw copy or copied bytes differ")
    return {"before": before, "after": after, "copied_before_overlay": copied}


def checked_binding(binding, files, kind):
    revision = binding.get("source_revision", "")
    if binding.get("version") != 1 or binding.get("clean") is not True or len(revision) != 40 or any(c not in "0123456789abcdef" for c in revision):
        raise ValueError("clean exact source binding required")
    if binding.get("kind") != kind:
        raise ValueError("binding operation mismatch")
    for name, value in files.items():
        if binding.get(name+"_sha256") != value:
            raise ValueError(f"reviewed file identity mismatch: {name}")


def invoke(command, root, name, timeout):
    child = {"command": [str(x) for x in command], "started": anchor()}
    with (root/(name+".stdout")).open("xb") as out, (root/(name+".stderr")).open("xb") as err:
        process = subprocess.Popen(command, stdout=out, stderr=err, env={**os.environ, "OMP_NUM_THREADS":"1"})
        child["pid"] = process.pid
        try:
            child["returncode"] = process.wait(timeout=timeout)
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
            child["finished"] = anchor()
            exclusive(root/(name+"-process.json"), child)
    if child["returncode"] != 0:
        raise ValueError(f"child failed: {name}")
    return child


def prepare(args):
    args.output.mkdir(parents=False, exist_ok=False)
    receipt = {"version":1,"complete":False,"started":anchor(),"source_catalog":str(args.source_catalog),
               "copied_catalog":str(args.output/"catalog/catalog.sqlite3"),"source_sqlite_connections":0}
    error = None
    expected = None
    try:
        paths = {name:getattr(args,name) for name in ("binary","archive","storage","source_binding","dataset")}
        paths.update(coordinator=Path(__file__),protocol=Path(__file__).resolve().parents[1]/"docs/PREVIEW_INTEGRATED_PROTOCOL.md")
        files = {k:digest(v) for k,v in paths.items()}
        binding = read_json(args.binding,65536)
        checked_binding(binding,files,"prepare")
        source = read_json(args.source_binding,65536)
        if source.get("catalog_count") != 10000000 or source.get("schema_version") != 4 or source.get("source_catalog") != str(args.source_catalog):
            raise ValueError("exact schema-4 10M source binding required")
        expected = source["files"]
        source_revision=source.get("source_revision", "")
        if len(source_revision)!=40 or any(c not in "0123456789abcdef" for c in source_revision):
            raise ValueError("original producer source revision required")
        provenance=source.get("receipts", {})
        if set(provenance)!={"build_reference", "preparation", "source_verification"}:
            raise ValueError("original producer and invariance receipts required")
        for entry in provenance.values():
            if digest(Path(entry["path"]))!=entry["sha256"]:
                raise ValueError("original provenance receipt changed")
        if args.output.resolve().is_relative_to(args.source_catalog.parent.resolve()):
            raise ValueError("output must be outside the original source directory")
        if set(expected) != set(SUFFIXES):
            raise ValueError("all companion presence identities required")
        minimum = binding.get("minimum_free_bytes")
        data=read_json(args.dataset,1024*1024)
        cache_bytes=data.get("store",{}).get("thumbnail_bytes")
        if type(cache_bytes) is not int or cache_bytes<=0:
            raise ValueError("retained cache footprint admission missing")
        required = sum(v.get("bytes",0) for v in expected.values()) + cache_bytes + 2*1024**3
        if type(minimum) is not int or minimum < required or shutil.disk_usage(args.output).free < minimum:
            raise ValueError("reviewed copy/cache/filesystem headroom unavailable")
        receipt["identities"] = files
        receipt["binding_sha256"] = digest(args.binding)
        receipt.update(raw_copy(args.source_catalog,args.output/"catalog/catalog.sqlite3",expected))
        receipt["complete"] = True
    except BaseException as exc:
        error = exc
        receipt["error"] = f"{type(exc).__name__}: {exc}"
    finally:
        if expected is not None:
            try:
                receipt["source_after_attempt"]=source_state(args.source_catalog)
                receipt["source_preserved"]=receipt["source_after_attempt"]==expected
                receipt["complete"] &= receipt["source_preserved"]
                if not receipt["source_preserved"]:
                    error=error or ValueError("source changed during copy attempt")
            except (OSError,ValueError) as exc:
                receipt["complete"]=False
                receipt["invariance_error"]=str(exc)
                error=error or exc
        receipt["finished"] = anchor()
        exclusive(args.output/"copy-receipt.json",receipt)
    if error is not None:
        raise error
    # An interrupted/failed overlay is retained; this command never reuses that bundle.
    overlay_error = None
    final = {"version":1,"complete":False,"started":anchor()}
    try:
        final["child"] = invoke([str(args.binary),"overlay","--bundle",str(args.output),"--dataset",str(args.dataset)],args.output,"overlay",3600)
        overlay = read_json(args.output/"overlay-receipt.json",1024*1024)
        if overlay.get("complete") is not True or overlay.get("overlay",{}).get("connection_total_changes_delta") != 20000:
            raise ValueError("overlay proof incomplete")
        final["overlay_sha256"] = digest(args.output/"overlay-receipt.json")
        final["complete"] = True
    except BaseException as exc:
        overlay_error = exc
        final["error"] = f"{type(exc).__name__}: {exc}"
    finally:
        try:
            final["source_preserved"] = source_state(args.source_catalog) == expected
            final["bound_files_preserved"] = all(digest(p)==files[k] for k,p in paths.items())
        except (OSError,ValueError) as exc:
            final["source_preserved"]=False
            final["bound_files_preserved"]=False
            final["invariance_error"]=str(exc)
        final["complete"] &= final["source_preserved"] and final["bound_files_preserved"]
        final["finished"] = anchor()
        exclusive(args.output/"preparation.json",final)
    if overlay_error is not None:
        raise overlay_error
    if not final["complete"]:
        raise ValueError("source/bound file invariance failed")


def run(args):
    args.output.mkdir(parents=False, exist_ok=False)
    root = Path(__file__).resolve().parents[1]
    receipt = {"version":1,"complete":False,"started":anchor(),"children":[],"metadata_count":10000000,
               "planned_measured_children":2,"planned_verifiers":2,"automatic_retries":0,
               "desktop_frame_time":"unavailable; S12","quietness_verified":False}
    error = None
    paths, frozen, expected = {}, {}, None
    source_catalog = None
    try:
        fixture = read_json(args.fixture,65536)
        bundle = args.fixture.parent
        paths = {k:getattr(args,k) for k in ("binary","worker","archive","storage","fixture","source_binding")}
        paths.update(dataset=Path(fixture["dataset"]),overlay=bundle/"overlay-receipt.json",copy=bundle/"copy-receipt.json",preparation=bundle/"preparation.json",coordinator=Path(__file__),protocol=root/"docs/PREVIEW_INTEGRATED_PROTOCOL.md",navigation_coordinator=root/"scripts/preview_navigation_campaign.py")
        frozen = {k:digest(v) for k,v in paths.items()}
        binding = read_json(args.binding,65536)
        checked_binding(binding,frozen,"run")
        if binding.get("planned_measured_children") != 2 or binding.get("planned_verifiers") != 2:
            raise ValueError("fixed two-profile plan required")
        source = read_json(args.source_binding,65536)
        expected = source["files"]
        source_catalog = Path(source["source_catalog"])
        if source_state(source_catalog) != expected:
            raise ValueError("original source changed since frozen preparation")
        data = read_json(fixture["dataset"],1024*1024)
        if fixture.get("catalog_count") != 10000000 or fixture.get("count") != 10000 or data.get("id_scheme") != "organization_fixture" or fixture.get("offline_originals") != "/synthetic" or Path("/synthetic").exists():
            raise ValueError("wrong integrated fixture or actual sources online")
        if any(read_json(paths[k],1024*1024).get("complete") is not True for k in ("copy","overlay","preparation")):
            raise ValueError("incomplete preparation")
        receipt["identities"] = frozen
        receipt["binding_sha256"] = digest(args.binding)
        receipt["host"] = host_identity(args.output,[args.fixture,Path(fixture["catalog"])])
        exclusive(args.output/"preflight.json",receipt)
        with HostObservation(args.output) as observer:
            for profile in ("standard","constrained"):
                folder = args.output/profile
                child = {"profile":profile,"complete":False}
                receipt["children"].append(child)
                child["process"] = invoke([str(args.binary),"run","--fixture",str(args.fixture),"--worker",str(args.worker),"--output",str(folder),"--profile",profile,"--workload","warm"],args.output,profile,900)
                result = read_json(folder/"receipt.json",1024*1024)
                child["receipt_sha256"] = digest(folder/"receipt.json")
                child["result"] = result
                if result.get("complete") is not True or result.get("profile") != profile or result.get("workload") != "warm" or result.get("catalog_count") != 10000000 or result.get("catalog_schema") != 4 or result.get("actual_preserved_source_paths_verified") is not True or result.get("native_jobs") != 0 or result.get("dataset_blake3") != fixture["dataset_blake3"]:
                    raise ValueError("child count/schema/offline/key/native proof failed")
                child["verifier"] = invoke([str(args.binary),"verify",str(folder)],args.output,profile+"-verify",60)
                if read_json(args.output/(profile+"-verify.stdout"),65536).get("complete") is not True:
                    raise ValueError("receipt chain verification failed")
                expected_trials_list=expected_trials("warm")
                entries=result.get("trials",[])
                if len(entries)!=len(expected_trials_list):
                    raise ValueError("wrong fixed sample count")
                observed=[]
                for entry,(kind,index) in zip(entries,expected_trials_list):
                    name=f"{kind}-{index:03}.json"
                    if entry.get("path")!=name:
                        raise ValueError("wrong trial identity")
                    row=validate_trial(read_json(folder/name),kind,index)
                    observed.append({"kind":kind,"index":index,"wall_ms":row["wall_ms"],"peak_resident_bytes":row["peak_resident_bytes"],"sha256":digest(folder/name)})
                child["observations"]=observed
                child["distributions"]={kind:distribution([r["wall_ms"] for r in observed if r["kind"]==kind]) for kind in ("warmup","warm","hot_lru")}
                child["peak_resident_bytes"]=max([r["peak_resident_bytes"] for r in observed]+[result["peak_resident_bytes"]])
                child["warm_headless_p95_within_1000ms"]=child["distributions"]["warm"]["p95"]<=1000
                child["integrated_rss_within_4gib"]=child["peak_resident_bytes"]<=4*1024**3
                child["complete"]=True
            receipt["telemetry"]=observer.finish()
            if receipt["telemetry"].get("complete") is not True:
                raise ValueError("incomplete telemetry")
        receipt["complete"]=True
    except BaseException as exc:
        error=exc
        receipt["error"]=f"{type(exc).__name__}: {exc}"
    finally:
        try:
            receipt["source_preserved"]=expected is not None and source_catalog is not None and source_state(source_catalog)==expected
            receipt["bound_files_preserved"]=bool(frozen) and all(digest(paths[k])==v for k,v in frozen.items())
        except (OSError,ValueError) as exc:
            receipt["invariance_error"]=str(exc)
            receipt["source_preserved"]=False
        receipt["complete"] &= receipt.get("source_preserved",False) and receipt.get("bound_files_preserved",False)
        receipt["finished"]=anchor()
        exclusive(args.output/"campaign.json",receipt)
    if error is not None:
        raise error
    if not receipt["complete"]:
        raise ValueError("source/binding invariance failed")


if __name__ == "__main__":
    parser=argparse.ArgumentParser()
    sub=parser.add_subparsers(dest="command",required=True)
    for operation in ("prepare","run"):
        command=sub.add_parser(operation)
        fields=("binary","archive","storage","source-binding","binding","output")
        fields += ("source-catalog","dataset") if operation=="prepare" else ("worker","fixture")
        for name in fields:
            command.add_argument("--"+name,type=Path,required=True)
        command.add_argument("--lane-token",required=True)
    args=parser.parse_args()
    if args.lane_token!="coordinator-authorized":
        parser.error("explicit coordinator lane required")
    for value in vars(args).values():
        if isinstance(value,Path) and not value.is_absolute():
            parser.error("absolute private paths required")
    (prepare if args.command=="prepare" else run)(args)
