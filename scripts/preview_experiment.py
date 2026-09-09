#!/usr/bin/env python3
"""Protocol-1 coordinator. No engine tuning; all outputs are private/exclusive.

Only `run` performs rendering/timing and requires the coordinator's lane token.
A manifest has version=1 and exactly30 inputs: id,path,sha256,width,height,
kind (private/public/preparation); six preparation entries also name an oracle.
"""
from __future__ import annotations
import argparse
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import random
import subprocess
import time
import psutil

EDGES = (256, 512, 1600, 2560)
QUALITIES = {"jpeg": (50, 65, 80), "webp": (50, 65, 80), "avif": (45, 60, 75)}
SEED = 22841

def digest(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()

def exclusive(path, value):
    with Path(path).open("x", encoding="utf-8") as stream:
        json.dump(value, stream, indent=2, allow_nan=False)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())

def utc():
    return datetime.now(timezone.utc).isoformat()

def stats(samples):
    if not samples or any(not isinstance(v,(float,int)) or isinstance(v,bool) or not math.isfinite(v) or v<=0 for v in samples):
        raise ValueError("invalid timing samples")
    ordered=sorted(samples)
    def percentile(q):
        position=(len(ordered)-1)*q
        lo=math.floor(position);hi=math.ceil(position)
        return ordered[lo]+(ordered[hi]-ordered[lo])*(position-lo)
    return {"n":len(samples),"p50_ms":percentile(.5),"p95_ms":percentile(.95),"p99_ms":percentile(.99),"max_ms":ordered[-1]}

def validate_case(result,edge,codec,quality,surface,folder):
    if result.get("complete") is not True or result.get("version")!=1 or result.get("edge")!=edge:
        raise ValueError("incomplete/wrong codec receipt")
    if (result.get("width"),result.get("height")) != (surface["width"],surface["height"]) or result.get("prepared_blake3")!=surface["rgb_blake3"]:
        raise ValueError("case prepared surface mismatch")
    if result["settings"]["codec"]!=codec or result["settings"]["quality"]!=quality:
        raise ValueError("case settings mismatch")
    for name,count in (("encode",3),("decode",20)):
        measured=result[name]
        recomputed=stats(measured["samples_ms"])
        if recomputed["n"]!=count or any(not math.isclose(measured[key],value,rel_tol=1e-12,abs_tol=1e-12) for key,value in recomputed.items()):
            raise ValueError("case sample count/distribution mismatch")
    artifacts=result["artifacts"]
    if len(artifacts)!=3:
        raise ValueError("all three encoded outputs required")
    for n,artifact in enumerate(artifacts):
        suffix="jpg" if codec=="jpeg" else codec
        path=folder/f"encode-{n}.{suffix}"
        if path.stat().st_size!=artifact["bytes"] or artifact["bytes"]<=0:
            raise ValueError("encoded artifact size mismatch")
        artifact["independent_sha256"]=digest(path)
        if codec=="jpeg":
            sampling=artifact["jpeg_sampling"]
            if sampling["precision"]!=8 or len(sampling["components"])!=3 or any(c["horizontal"]!=1 or c["vertical"]!=1 for c in sampling["components"]):
                raise ValueError("JPEG sampling differs from frozen444 configuration")
    metrics=result["quality"]
    if not math.isfinite(metrics["mse_rgb8"]) or metrics["mse_rgb8"]<0 or not math.isfinite(metrics["block_rgb_ssim"]):
        raise ValueError("invalid quality metric")
    return artifacts

def validate_manifest(value):
    if value.get("version") != 1 or len(value.get("inputs", [])) != 30:
        raise ValueError("protocol1 requires exactly30 frozen inputs")
    ids = set()
    kinds = {"private": 0, "public": 0, "preparation": 0}
    for item in value["inputs"]:
        key = item["id"]
        if not key or any(c not in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_" for c in key) or key in ids:
            raise ValueError("duplicate or unsafe fixture ID")
        ids.add(key)
        kinds[item["kind"]] += 1
        if not isinstance(item.get("group"),str) or not item["group"]:
            raise ValueError("camera/format group is required")
        if len(item["sha256"]) != 64 or any(c not in "0123456789abcdef" for c in item["sha256"]):
            raise ValueError("invalid input digest")
        if not all(isinstance(item[x], int) and item[x] > 0 for x in ("width", "height")):
            raise ValueError("invalid dimensions")
        if item["kind"] == "preparation" and not item.get("oracle"):
            raise ValueError("preparation fixture needs independent oracle")
        if item["kind"] == "preparation" and set(item["oracle"]) != {str(x) for x in EDGES}:
            raise ValueError("all four preparation oracle edges are required")
    if kinds != {"private":22,"public":2,"preparation":6}:
        raise ValueError("incorrect frozen corpus composition")

def child(command, prefix):
    """File I/O/metrics outside child codec timers; sample complete process RSS."""
    start = time.monotonic()
    with prefix.with_suffix(".stdout").open("xb") as out, prefix.with_suffix(".stderr").open("xb") as err:
        process = subprocess.Popen(command, stdout=out, stderr=err, env={**os.environ,"OMP_NUM_THREADS":"1"})
        observed = psutil.Process(process.pid)
        peak = 0
        cpu = None
        unavailable = []
        try:
            while process.poll() is None:
                try:
                    peak = max(peak, observed.memory_info().rss)
                    used = observed.cpu_times()
                    cpu = used.user + used.system
                except psutil.NoSuchProcess:
                    pass
                except psutil.Error as exc:
                    unavailable.append(type(exc).__name__)
                time.sleep(0.01)
        except BaseException:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            raise
        return {"returncode":process.returncode,"wall_seconds":time.monotonic()-start,
                "peak_rss_bytes":peak or None,"cpu_seconds_last_sample":cpu,
                "observation_errors":sorted(set(unavailable)),"rss_sample_interval_ms":10}

def run(args):
    if args.lane_token != "coordinator-authorized":
        raise ValueError("timed/rendering lane must be explicitly authorized")
    manifest_path = Path(args.manifest).resolve()
    manifest = json.loads(manifest_path.read_text())
    validate_manifest(manifest)
    output = Path(args.output).resolve()
    binary = Path(args.binary).resolve()
    # Never place experiment output inside source directories or the evidence input.
    for item in manifest["inputs"]:
        path = Path(item["path"]).resolve()
        if output == path or output in path.parents or path.parent == output or path.parent in output.parents:
            raise ValueError("output overlaps an input source directory")
    output.mkdir(exist_ok=False)
    source = {item["id"]: digest(item["path"]) for item in manifest["inputs"]}
    if any(source[x["id"]] != x["sha256"] for x in manifest["inputs"]):
        exclusive(output/"preflight-failed.json", {"error":"source digest mismatch"})
        raise ValueError("source digest mismatch")
    for item in manifest["inputs"]:
        for expected in item.get("oracle",{}).values():
            if digest(expected["path"]) != expected["sha256"]:
                exclusive(output/"oracle-preflight-failed.json", {"error":"oracle digest mismatch","id":item["id"]})
                raise ValueError("oracle digest mismatch")
    exclusive(output/"manifest.json", manifest)
    identity = {"version":1,"started_utc":utc(),"binary_sha256":digest(binary),
                "runner_sha256":digest(__file__),"manifest_sha256":digest(manifest_path),
                "host":{"platform":os.uname().sysname if hasattr(os,"uname") else os.name,
                        "cpus":psutil.cpu_count(),"ram_bytes":psutil.virtual_memory().total},
                "seed":SEED,"settings":{"edges":EDGES,"qualities":QUALITIES,"encode_warmups":1,"encode_repetitions":3,"decode_warmups":3,"decode_repetitions":20}}
    versions = subprocess.run([str(binary),"versions"],capture_output=True,text=True,check=True)
    identity["codec_versions"] = json.loads(versions.stdout)
    exclusive(output/"identity.json",identity)
    outcomes=[]
    items=list(manifest["inputs"])
    random.Random(SEED).shuffle(items)
    for item in items:
        fixture = output/item["id"]
        fixture.mkdir()
        prepared=fixture/"prepared"
        preparation=child([str(binary),"prepare",item["path"],str(prepared)],fixture/"prepare")
        record={"id":item["id"],"preparation":preparation,"cases":[]}
        if preparation["returncode"] == 0:
            result=json.loads((prepared/"receipt.json").read_text())
            if result.get("identity")!=identity["codec_versions"]:
                record["error"]="compiled preparation identity mismatch"
            if (result["source_width"],result["source_height"]) != (item["width"],item["height"]):
                record["error"]="source dimensions disagree with independent reference"
            for edge,expected in item.get("oracle",{}).items():
                actual=result["surfaces"][edge]
                if (actual["width"],actual["height"]) != (expected["width"],expected["height"]) or digest(prepared/f"{edge}.rgb") != expected["sha256"]:
                    record["error"]="prepared RGB8 disagrees with independent oracle"
            if "error" not in record:
                matrix=[(edge,codec,q) for edge in EDGES for codec,qualities in QUALITIES.items() for q in qualities]
                random.Random(f"{SEED}:{item['id']}").shuffle(matrix)
                for edge,codec,q in matrix:
                    name=f"{edge}-{codec}-{q}"
                    info=child([str(binary),"measure","--prepared",str(prepared),"--edge",str(edge),"--codec",codec,"--quality",str(q),"--output",str(fixture/name)],fixture/name)
                    info.update({"edge":edge,"codec":codec,"quality":q,"receipt":f"{item['id']}/{name}/receipt.json"})
                    if info["returncode"]==0:
                        try:
                            if not info["peak_rss_bytes"] or info["observation_errors"]:
                                raise ValueError("required process-memory observation unavailable")
                            case_result=json.loads((fixture/name/"receipt.json").read_text())
                            if case_result.get("identity")!=identity["codec_versions"]:
                                raise ValueError("compiled codec identity changed")
                            info["verified_artifacts"]=validate_case(case_result,edge,codec,q,result["surfaces"][str(edge)],fixture/name)
                        except (ValueError,KeyError,OSError,TypeError) as exc:
                            info["validation_error"]=str(exc)
                            info["returncode"]=-1
                    record["cases"].append(info)
                record["quality_review"]=[]
                for edge in EDGES:
                    cases=[c for c in record["cases"] if c["edge"]==edge]
                    if any(c["returncode"] for c in cases):
                        record["quality_review"].append({"edge":edge,"complete":False,"error":"candidate failure retained"})
                        continue
                    random.Random(f"{SEED}:blind:{item['id']}:{edge}").shuffle(cases)
                    review={"reference":str(prepared/f"{edge}.png"),"candidates":[]}
                    mapping=[]
                    for number,case in enumerate(cases):
                        label=chr(ord('A')+number)
                        review["candidates"].append({"label":label,"path":str(fixture/f"{edge}-{case['codec']}-{case['quality']}"/"decoded.png")})
                        mapping.append({"label":label,"codec":case["codec"],"quality":case["quality"]})
                    review_manifest=fixture/f"review-{edge}-inputs.json"
                    exclusive(review_manifest,review)
                    exclusive(fixture/f"review-{edge}-mapping.json",mapping)
                    observed=child([str(binary),"review",str(review_manifest),str(fixture/f"review-{edge}")],fixture/f"review-{edge}")
                    record["quality_review"].append({"edge":edge,"complete":observed["returncode"]==0,"observation":observed})
        exclusive(fixture/"summary.json",record)
        outcomes.append(record)
    after={item["id"]:digest(item["path"]) for item in manifest["inputs"]}
    binary_preserved=digest(binary)==identity["binary_sha256"]
    complete=binary_preserved and all(x["preparation"]["returncode"]==0 and "error" not in x and len(x["cases"])==36 and all(c["returncode"]==0 for c in x["cases"]) and len(x.get("quality_review",[]))==4 and all(r["complete"] for r in x["quality_review"]) for x in outcomes)
    aggregate={}
    for item,record in zip(items,outcomes):
        for case in record["cases"]:
            key=f"{case['edge']}-{case['codec']}-{case['quality']}"
            summary=aggregate.setdefault(key,{"files":[],"failed_ids":[]})
            if case["returncode"]:
                summary["failed_ids"].append(item["id"])
                continue
            result=json.loads((output/case["receipt"]).read_text())
            summary["files"].append({"id":item["id"],"group":item["group"],"encoded_bytes":result["artifacts"][0]["bytes"],"encode":result["encode"],"decode":result["decode"],"quality":result["quality"],"peak_child_rss_bytes":case["peak_rss_bytes"]})
    for summary in aggregate.values():
        sizes=[f["encoded_bytes"] for f in summary["files"]]
        summary["coverage"]={"successful":len(sizes),"expected":30}
        summary["sum_encoded_bytes"]=sum(sizes)
        summary["encoded_size_distribution"]=({k.replace('_ms','_bytes'):v for k,v in stats(sizes).items()} if sizes else None)
        summary["projected_bytes"]=({str(n):sum(sizes)*n/len(sizes) for n in (1_000_000,5_000_000,10_000_000)} if len(sizes)==30 else None)
        summary["projection_scope"]="equal-weight compatibility corpus, not library frequency; quality artifacts excluded"
        summary["rss_scope"]="whole measurement child including prepared input, encode/decode, metrics and PNG output; not isolated codec stage"
    exclusive(output/"aggregate.json",aggregate)
    exclusive(output/"campaign.json",{"version":1,"complete":complete,"binary_preserved":binary_preserved,"source_preserved":source==after,"finished_utc":utc(),"outcomes":outcomes,"selection":"not evaluated; quality/layout/interaction review required"})
    if not complete or source != after:
        raise RuntimeError("campaign contains failures; retained receipts must be reviewed")

if __name__ == "__main__":
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest",required=True)
    parser.add_argument("--binary",required=True)
    parser.add_argument("--output",required=True)
    parser.add_argument("--lane-token",required=True)
    run(parser.parse_args())
