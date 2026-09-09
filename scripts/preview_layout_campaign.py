#!/usr/bin/env python3
"""Fixed flat/prefix experiment; no implicit layout winner or destructive cleanup."""
from __future__ import annotations
import argparse
import os
from pathlib import Path
import shutil
import subprocess

from preview_experiment import digest, exclusive
from preview_host import HostObservation, host_identity
from preview_navigation_campaign import anchor, distribution, read_json


def plan():
    return [(count, layout) for count in (10000, 100000) for layout in ("flat", "hash-prefix")]


def validate_lookup(value, count):
    if value.get("complete") is not True or not value.get("dataset_blake3"):
        raise ValueError("incomplete/unbound layout lookup")
    passes = value.get("passes", [])
    if len(passes) != 6:
        raise ValueError("all fixed passes required")
    for index, row in enumerate(passes):
        seed = None if index < 3 else 22841 + index - 3
        order = "sequential" if index < 3 else "seeded_random"
        if (row.get("pass"), row.get("seed"), row.get("order")) != (index, seed, order):
            raise ValueError("pass identity/order changed")
        if row.get("complete") is not True or row.get("lookup_count") != count or row.get("distinct_actual_read_content_hashes") != count:
            raise ValueError("actual read cardinality not proven")
        if row.get("bytes_read_verified", 0) <= count * 37:
            raise ValueError("actual seed payload bytes missing")
        if row.get("samples_file") != f"pass-{index}-samples.json":
            raise ValueError("unexpected sample artifact path")
    for phase in ("footprint_before", "footprint_after"):
        if value.get(phase, {}).get("thumbnail", {}).get("object_files") != count:
            raise ValueError("actual filesystem entry count mismatch")


def compare(preparations, lookups):
    expected = set(plan())
    if len(preparations) != 4 or len(lookups) != 4:
        raise ValueError("all four groups required before comparison")
    prepared = {(r["count"], r["layout"]): r for r in preparations}
    measured = {(r["count"], r["layout"]): r for r in lookups}
    if set(prepared) != expected or set(measured) != expected:
        raise ValueError("missing/duplicate group")
    comparison = {}
    for count in (10000, 100000):
        by_layout = {}
        payload_sizes = set()
        for layout in ("flat", "hash-prefix"):
            pre, lookup = prepared[count, layout], measured[count, layout]
            if pre.get("complete") is not True or lookup.get("complete") is not True:
                raise ValueError("incomplete comparison group")
            validate_lookup(lookup["result"], count)
            size = pre["result"]["encoded_object_bytes"]
            payload_sizes.add(size)
            by_layout[layout] = {"encoded_object_bytes":size,"marker_bytes":count*37,
                "footprint_preparation":pre["result"]["footprint"],
                "footprint_after_lookup":lookup["result"]["footprint_after"],
                "pass_distributions":lookup["distributions"],
                "pass_wall_and_oracle": [{"pass":p["pass"],"wall_ms":p["wall_ms"],
                    "independent_verification_ms":p["independent_verification_ms"]}
                    for p in lookup["result"]["passes"]]}
        if len(payload_sizes) != 1:
            raise ValueError("layouts did not store byte-equivalent payload totals")
        comparison[str(count)] = by_layout
    return comparison


def invoke(binary, command, output, record, timeout):
    record["started"] = anchor()
    with (output/(record["name"]+".stdout")).open("xb") as out, (output/(record["name"]+".stderr")).open("xb") as err:
        child = subprocess.Popen([str(binary), *command], stdout=out, stderr=err,
                                 env={**os.environ, "OMP_NUM_THREADS":"1"})
        record["pid"] = child.pid
        try:
            record["returncode"] = child.wait(timeout=timeout)
        finally:
            if child.poll() is None:
                child.kill()
                child.wait()
            record["finished"] = anchor()
    return record["returncode"]


def run(args):
    if args.lane_token != "coordinator-authorized":
        raise ValueError("explicit reviewed lane required")
    args.output.mkdir(parents=False, exist_ok=False)
    root = Path(__file__).resolve().parents[1]
    campaign = {"version":1,"complete":False,"started":anchor(),"preparations":[],"lookups":[],
                "planned_preparation_children":4,"planned_lookup_children":4,"automatic_retries":0,
                "quietness_verified":False,"layout_selected":None,
                "scope":"30-image diversity; distinct encoded objects by fixed lossless COM construction"}
    error = None
    frozen = {}
    seed_before = {}
    try:
        paths = {name:getattr(args,name) for name in ("binary","archive","storage","worker_campaign")}
        paths.update(protocol=root/"docs/PREVIEW_STAGE_B_PROTOCOL.md",coordinator=Path(__file__))
        frozen = {name:digest(path) for name,path in paths.items()}
        binding = read_json(args.binding,65536)
        if binding.get("version") != 1 or binding.get("clean") is not True or binding.get("planned_preparation_children") != 4 or binding.get("planned_lookup_children") != 4:
            raise ValueError("reviewed clean fixed-plan binding required")
        revision = binding.get("source_revision", "")
        if len(revision) != 40 or any(c not in "0123456789abcdef" for c in revision):
            raise ValueError("exact clean source revision required")
        if any(binding.get(name+"_sha256") != value for name,value in frozen.items()):
            raise ValueError("reviewed file identity mismatch")
        minimum = binding.get("minimum_free_bytes")
        if type(minimum) is not int or minimum <= 0:
            raise ValueError("explicit storage admission required")
        worker = read_json(args.worker_campaign,2*1024*1024)
        if worker.get("complete") is not True or worker.get("source_preserved") is not True or len(worker.get("children",[])) != 30:
            raise ValueError("complete preserved worker cohort required")
        # The probe verifies full BLAKE3/decode identities. This independent SHA
        # snapshot binds the actual 30 selected input files through the campaign.
        seed_paths = []
        for child in worker["children"]:
            identity = child["id"]
            if not identity or any(c not in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_" for c in identity):
                raise ValueError("unsafe seed identity")
            if child.get("complete") is not True or child.get("result", {}).get("fixture_id") != identity:
                raise ValueError("incomplete/wrong worker child")
            path = args.worker_campaign.parent/identity/"512.jpg"
            selected = [a for a in child.get("encoded_artifacts", []) if a.get("path") == "512.jpg"]
            if len(selected) != 1 or digest(path) != selected[0].get("sha256"):
                raise ValueError("selected input bytes differ from independently verified worker output")
            seed_paths.append(path)
        if len(set(seed_paths)) != 30:
            raise ValueError("duplicate seed identity")
        seed_before = {str(p):digest(p) for p in seed_paths}
        campaign["input_seeds"] = seed_before
        campaign["identities"] = frozen
        campaign["binding_sha256"] = digest(args.binding)
        campaign["minimum_free_bytes"] = minimum
        lengths = [p.stat().st_size + 37 for p in seed_paths]
        projected = sum(sum(lengths[i % 30] for i in range(count)) for count, _ in plan())
        campaign["projected_encoded_bytes"] = projected
        campaign["fixed_marker_bytes"] = 37 * sum(count for count, _ in plan())
        if minimum < projected + 2*1024**3:
            raise ValueError("reviewed storage admission must cover encoded projection plus 2 GiB explicit overhead headroom")
        campaign["free_bytes_before"] = shutil.disk_usage(args.output).free
        if campaign["free_bytes_before"] < minimum:
            raise ValueError("reviewed filesystem free-space admission not met")
        campaign["host"] = host_identity(args.output,seed_paths)
        exclusive(args.output/"preflight.json",campaign)
        with HostObservation(args.output) as observer:
            # Preparation completes for all four independent filesets before
            # lookup children begin. OS caches are never purged or called cold.
            for count,layout in plan():
                name=f"{count}-{layout}"
                folder=args.output/name
                row={"name":name,"count":count,"layout":layout,"complete":False}
                campaign["preparations"].append(row)
                code=invoke(args.binary,["prepare","--campaign",str(args.worker_campaign),"--output",str(folder),"--count",str(count),"--layout",layout],args.output,row,3600)
                if (folder/"preparation.json").exists():
                    row["result"]=read_json(folder/"preparation.json",2*1024*1024)
                    row["receipt_sha256"]=digest(folder/"preparation.json")
                if code != 0:
                    raise ValueError(f"preparation failed: {name}")
                result=row["result"]
                if result.get("complete") is not True or result.get("entries_completed") != count or result.get("distinct_generated_content_hashes") != count or result.get("footprint",{}).get("thumbnail",{}).get("object_files") != count:
                    raise ValueError("prepared actual distinct-object contract failed")
                row["dataset_sha256"]=digest(folder/"dataset.json")
                row["complete"]=True
                exclusive(args.output/(name+"-preparation-child.json"),row)
            for count,layout in plan():
                name=f"{count}-{layout}"
                folder=args.output/(name+"-lookup")
                row={"name":name+"-lookup","count":count,"layout":layout,"complete":False}
                campaign["lookups"].append(row)
                code=invoke(args.binary,["lookup","--dataset",str(args.output/name/"dataset.json"),"--output",str(folder)],args.output,row,900)
                if (folder/"lookup.json").exists():
                    row["result"]=read_json(folder/"lookup.json",2*1024*1024)
                    row["receipt_sha256"]=digest(folder/"lookup.json")
                if code != 0:
                    raise ValueError(f"lookup failed: {name}")
                validate_lookup(row["result"],count)
                row["distributions"]=[]
                for index in range(6):
                    path=folder/f"pass-{index}-samples.json"
                    samples=read_json(path,16*1024*1024)
                    if not isinstance(samples,list) or len(samples)!=count:
                        raise ValueError("raw lookup count mismatch")
                    row["distributions"].append({"pass":index,"sha256":digest(path),"lookup_ms":distribution(samples)})
                row["complete"]=True
                exclusive(args.output/(name+"-lookup-child.json"),row)
            campaign["comparisons"] = compare(campaign["preparations"], campaign["lookups"])
            for row in campaign["preparations"]:
                if digest(args.output/row["name"]/"dataset.json") != row["dataset_sha256"]:
                    raise ValueError("prepared dataset descriptor changed")
            campaign["telemetry"]=observer.finish()
            if campaign["telemetry"].get("complete") is not True:
                raise ValueError("host telemetry incomplete")
        campaign["seed_files_preserved"]=all(digest(Path(path))==sha for path,sha in seed_before.items())
        if not campaign["seed_files_preserved"]:
            raise ValueError("selected seed file changed")
        campaign["complete"]=True
    except BaseException as exc:
        error=exc
        campaign["error"]=f"{type(exc).__name__}: {exc}"
    finally:
        if seed_before:
            try:
                campaign["seed_files_preserved"] = all(digest(Path(path)) == sha for path, sha in seed_before.items())
            except OSError:
                campaign["seed_files_preserved"] = False
            if not campaign["seed_files_preserved"]:
                campaign["complete"] = False
                error = error or ValueError("selected seed file changed")
                campaign.setdefault("error", str(error))
        if frozen:
            try:
                campaign["bound_files_preserved"]=all(digest(paths[name])==value for name,value in frozen.items())
            except OSError:
                campaign["bound_files_preserved"]=False
            if not campaign["bound_files_preserved"]:
                campaign["complete"]=False
                error=error or ValueError("bound source inputs changed")
                campaign.setdefault("error",str(error))
        campaign["finished"]=anchor()
        campaign["free_bytes_after"]=shutil.disk_usage(args.output).free
        exclusive(args.output/"campaign.json",campaign)
    if error is not None:
        raise error


if __name__=="__main__":
    parser=argparse.ArgumentParser()
    for name in ("binary","archive","storage","worker-campaign","binding","output"):
        parser.add_argument(f"--{name}",type=Path,required=True)
    parser.add_argument("--lane-token",required=True)
    run(parser.parse_args())
