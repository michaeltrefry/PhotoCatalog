#!/usr/bin/env python3
"""Read-only work diagnostics, independent of latency eligibility (protocol 1)."""

import argparse
import ctypes as C
import hashlib
import json
from pathlib import Path
import platform
import sys

import _sqlite3
import duckdb

import catalog_benchmark as bench

VERSION = 1
SCALES = (1_000_000, 5_000_000, 10_000_000)
CURSORS = (50, 90)
WORKLOADS = ("page_deep", "rating")
FROZEN_SHA256 = "167b13d524c0f21a18426bb8b21acc6792f275de7884edca1cf995c6ff874c89"


def require(value, message):
    if not value:
        raise ValueError(message)


def digest(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def integer(value, name, positive=False):
    require(type(value) is int and value >= int(positive), f"invalid {name}")
    return value


def validate_sqlite_metrics(metrics):
    integer(metrics.get("vm_step"), "VM_STEP", positive=True)
    for key in ("sort", "fullscan_step"):
        integer(metrics.get(key), key)
    available = metrics.get("scanstatus_available")
    require(type(available) is bool, "missing scanstatus availability")
    loops = metrics.get("scan_loops")
    require(isinstance(loops, list), "missing scan loop list")
    if available:
        require(bool(loops), "available scanstatus returned no loops")
        for loop in loops:
            integer(loop.get("nvisit"), "NVISIT")
            integer(loop.get("nloop"), "NLOOP")
            require(isinstance(loop.get("explain"), str) and loop["explain"], "missing scan explanation")
        require(sum(loop["nvisit"] for loop in loops) > 0, "no visited rows")
    else:
        require(not loops and bool(metrics.get("scanstatus_unavailable_reason")), "missing scanstatus fallback reason")
    return metrics


def validate_duck_profile(profile, sql, returned):
    require(isinstance(profile, dict), "profile must be an object")
    require(profile.get("query_name") == sql, "profile belongs to a different query")
    require(integer(profile.get("rows_returned"), "rows_returned") == returned, "profile row count mismatch")
    roots = profile.get("children")
    require(isinstance(roots, list) and roots, "profile lacks operator tree")
    operators = []

    def walk(node):
        require(isinstance(node, dict), "invalid operator")
        require(isinstance(node.get("operator_name"), str) and node["operator_name"], "missing operator name")
        require(isinstance(node.get("operator_type"), str) and node["operator_type"], "missing operator type")
        integer(node.get("operator_rows_scanned"), "operator_rows_scanned")
        integer(node.get("operator_cardinality"), "operator_cardinality")
        require(isinstance(node.get("children"), list), "incomplete operator children")
        operators.append(node)
        for child in node["children"]:
            walk(child)

    for node in roots:
        walk(node)
    scanned = sum(node["operator_rows_scanned"] for node in operators)
    require(integer(profile.get("cumulative_rows_scanned"), "cumulative_rows_scanned") == scanned, "incomplete/inconsistent scan tree")
    require(sum(node["operator_cardinality"] for node in roots) == returned, "root operator cardinality mismatch")
    return {"operator_rows_scanned_sum": scanned, "operator_cardinality_sum": sum(node["operator_cardinality"] for node in operators), "operators": len(operators), "interpretation": "engine-reported counters; zero rows scanned alone is not a bound on index work"}


def expected_page(name, parameters, count):
    """Independent generator oracle: verify first 200 qualifying rows, not just predicates."""
    cursor = parameters[-1]
    expected = []
    for sequence in range(cursor + 1, count + 1):
        row = bench.asset_row(sequence)
        if name == "rating" and row[11] != parameters[0]:
            continue
        expected.append([row[0], row[1], row[9], row[10], row[11], row[7]])
        if len(expected) == bench.PAGE_SIZE:
            break
    require(len(expected) == bench.PAGE_SIZE, "fixture cannot supply a 200-row page")
    return expected


def validate_rows(name, parameters, rows, count):
    expected = expected_page(name, parameters, count)
    require([list(row) for row in rows] == expected, "page differs from first 200 expected generated records")
    return {"rows": 200, "first_sequence": expected[0][0], "last_sequence": expected[-1][0], "oracle": "frozen generator; all six selected values and first qualifying page"}


class NativeSQLite:
    """Own native connection; never reach into CPython's private connection layout."""

    def __init__(self, path, memory_mb):
        self.lib = C.CDLL(_sqlite3.__file__)
        signatures = {
            "sqlite3_open_v2": ([C.c_char_p, C.POINTER(C.c_void_p), C.c_int, C.c_char_p], C.c_int),
            "sqlite3_close": ([C.c_void_p], C.c_int),
            "sqlite3_errmsg": ([C.c_void_p], C.c_char_p),
            "sqlite3_libversion": ([], C.c_char_p),
            "sqlite3_sourceid": ([], C.c_char_p),
            "sqlite3_compileoption_used": ([C.c_char_p], C.c_int),
            "sqlite3_db_readonly": ([C.c_void_p, C.c_char_p], C.c_int),
            "sqlite3_prepare_v2": ([C.c_void_p, C.c_char_p, C.c_int, C.POINTER(C.c_void_p), C.POINTER(C.c_char_p)], C.c_int),
            "sqlite3_bind_int64": ([C.c_void_p, C.c_int, C.c_int64], C.c_int),
            "sqlite3_step": ([C.c_void_p], C.c_int),
            "sqlite3_finalize": ([C.c_void_p], C.c_int),
            "sqlite3_column_count": ([C.c_void_p], C.c_int),
            "sqlite3_column_type": ([C.c_void_p, C.c_int], C.c_int),
            "sqlite3_column_int64": ([C.c_void_p, C.c_int], C.c_int64),
            "sqlite3_column_text": ([C.c_void_p, C.c_int], C.c_void_p),
            "sqlite3_column_bytes": ([C.c_void_p, C.c_int], C.c_int),
            "sqlite3_stmt_status": ([C.c_void_p, C.c_int, C.c_int], C.c_int),
        }
        for name, (arguments, result) in signatures.items():
            function = getattr(self.lib, name)
            function.argtypes, function.restype = arguments, result
        self.version = self.lib.sqlite3_libversion().decode()
        require(self.version == _sqlite3.sqlite_version, "native library differs from Python SQLite")
        self.source_id = self.lib.sqlite3_sourceid().decode()
        self.scan = getattr(self.lib, "sqlite3_stmt_scanstatus", None)
        if self.scan is not None:
            self.scan.argtypes = [C.c_void_p, C.c_int, C.c_int, C.c_void_p]
            self.scan.restype = C.c_int
        self.scan_available = self.scan is not None and bool(self.lib.sqlite3_compileoption_used(b"ENABLE_STMT_SCANSTATUS"))
        self.handle = C.c_void_p()
        uri = Path(path).resolve().as_uri() + "?mode=ro&immutable=1"
        code = self.lib.sqlite3_open_v2(uri.encode(), C.byref(self.handle), 1 | 0x40, None)
        try:
            self.check(code)
            require(self.lib.sqlite3_db_readonly(self.handle, b"main") == 1, "SQLite connection is not read-only")
            self.settings = bench.settings("sqlite", memory_mb).copy()
            # journal_mode is persistent: inspect it, never issue a setter on the source.
            self.settings.pop("journal_mode")
            self.settings["query_only"] = 1
            for key, value in self.settings.items():
                self.query(f"PRAGMA {key}={value}")
            self.actual_settings = {key: self.query(f"PRAGMA {key}")[0][0][0] for key in self.settings}
            require(self.actual_settings == self.settings, "SQLite settings readback mismatch")
        except BaseException:
            self.close()
            raise

    def check(self, code):
        if code != 0:
            raise RuntimeError(f"SQLite {code}: {self.lib.sqlite3_errmsg(self.handle).decode()}")

    def close(self):
        if self.handle:
            self.check(self.lib.sqlite3_close(self.handle))
            self.handle = C.c_void_p()

    def query(self, sql, parameters=(), metrics=False):
        statement = C.c_void_p()
        tail = C.c_char_p()
        encoded_sql = sql.encode()
        self.check(self.lib.sqlite3_prepare_v2(self.handle, encoded_sql, -1, C.byref(statement), C.byref(tail)))
        try:
            require(not tail.value, "multiple SQL statements are not allowed")
            for index, value in enumerate(parameters, 1):
                self.check(self.lib.sqlite3_bind_int64(statement, index, value))
            rows = []
            while True:
                code = self.lib.sqlite3_step(statement)
                if code == 101:  # SQLITE_DONE: metrics are sampled before finalize.
                    break
                if code != 100:
                    self.check(code)
                row = []
                for index in range(self.lib.sqlite3_column_count(statement)):
                    kind = self.lib.sqlite3_column_type(statement, index)
                    if kind == 1:
                        value = self.lib.sqlite3_column_int64(statement, index)
                    elif kind == 3:
                        ptr = self.lib.sqlite3_column_text(statement, index)
                        length = self.lib.sqlite3_column_bytes(statement, index)
                        value = C.string_at(ptr, length).decode()
                    elif kind == 5:
                        value = None
                    else:
                        raise ValueError(f"unexpected SQLite column type {kind}")
                    row.append(value)
                rows.append(row)
            work = None
            if metrics:
                work = {key: self.lib.sqlite3_stmt_status(statement, opcode, 0) for key, opcode in (("fullscan_step", 1), ("sort", 2), ("vm_step", 4))}
                work.update(scanstatus_available=self.scan_available, scan_loops=[])
                if self.scan_available:
                    for index in range(1000):
                        nloop, nvisit, explain = C.c_int64(), C.c_int64(), C.c_char_p()
                        if self.scan(statement, index, 0, C.byref(nloop)) != 0:
                            break
                        require(self.scan(statement, index, 1, C.byref(nvisit)) == 0, "missing NVISIT")
                        require(self.scan(statement, index, 4, C.byref(explain)) == 0, "missing scan explanation")
                        work["scan_loops"].append({"nloop": nloop.value, "nvisit": nvisit.value, "explain": explain.value.decode() if explain.value else None})
                    else:
                        raise ValueError("scanstatus loop limit exceeded")
                else:
                    work["scanstatus_unavailable_reason"] = "native library lacks SQLITE_ENABLE_STMT_SCANSTATUS or exported API; VM_STEP includes range-scan work that FULLSCAN_STEP misses"
                validate_sqlite_metrics(work)
            return rows, work
        finally:
            self.lib.sqlite3_finalize(statement)


def duck_query(db, sql, parameters, profile_path):
    require(not profile_path.exists(), "profile output exists")
    # The parent output directory was created exclusively. Only this query is profiled.
    quoted = str(profile_path).replace("'", "''")
    db.execute(f"SET profiling_output='{quoted}'")
    db.execute("SET custom_profiling_settings='{" + ','.join(f'"{key}":"true"' for key in ("QUERY_NAME", "ROWS_RETURNED", "CUMULATIVE_ROWS_SCANNED", "OPERATOR_NAME", "OPERATOR_ROWS_SCANNED", "OPERATOR_CARDINALITY", "EXTRA_INFO")) + "}'")
    db.execute("SET enable_profiling='json'")
    try:
        rows = db.execute(sql, parameters).fetchall()
    finally:
        db.execute("PRAGMA disable_profiling")
    profile = json.loads(profile_path.read_text())
    metrics = validate_duck_profile(profile, sql, len(rows))
    return rows, profile, metrics


def source_state(path):
    sidecars = [Path(str(path) + suffix) for suffix in ("-wal", "-shm", "-journal", ".wal", ".tmp")]
    require(not any(item.exists() for item in sidecars), "source must be checkpointed with no WAL/SHM/journal/temp sidecars")
    stat = path.stat()
    return {"bytes": stat.st_size, "mtime_ns": stat.st_mtime_ns, "inode": stat.st_ino, "sha256": digest(path)}


def run(snapshot, engine, count, memory_mb, output):
    snapshot, output = Path(snapshot).resolve(), Path(output).resolve()
    require(not output.is_relative_to(snapshot) and not snapshot.is_relative_to(output), "output must be separate from snapshot")
    require(engine in ("sqlite", "duckdb") and memory_mb > 0 and count > 0, "invalid engine/memory/count")
    manifest_path = snapshot / "snapshot.json"
    manifest = json.loads(manifest_path.read_text())
    require(manifest.get("complete") is True and count in manifest.get("counts", []), "incomplete snapshot or missing scale")
    require(digest(bench.__file__) == manifest.get("frozen_harness_sha256") == FROZEN_SHA256, "frozen harness identity mismatch")
    relative = Path(str(count)) / engine / ("catalog.sqlite3" if engine == "sqlite" else "catalog.duckdb")
    path = (snapshot / relative).resolve()
    require(path.is_relative_to(snapshot), "database escapes snapshot")
    artifact = manifest["snapshots"][str(relative)]
    output.mkdir(parents=True, exist_ok=False)
    receipt = {"version": VERSION, "complete": False, "diagnostic_only": True, "latency_eligibility": "not evaluated", "engine": engine, "count": count, "memory_mib": memory_mb, "python": sys.version, "platform": platform.platform(), "script_sha256": digest(__file__), "frozen_harness_sha256": FROZEN_SHA256, "snapshot_manifest_sha256": digest(manifest_path), "database": str(path), "queries": []}
    db = None
    before = None
    try:
        before = source_state(path)
        receipt["source_before"] = before
        require(before["sha256"] == artifact["sha256"] and before["bytes"] == artifact["bytes"], "database differs from preserved snapshot")
        if engine == "sqlite":
            db = NativeSQLite(path, memory_mb)
            receipt.update(engine_version=db.version, sqlite_source_id=db.source_id, native_library=_sqlite3.__file__, settings=db.actual_settings, open_mode="SQLITE_OPEN_READONLY | URI; immutable=1; query_only=1")
        else:
            options = bench.settings(engine, memory_mb)
            options["temp_directory"] = str(output / "spill")
            db = duckdb.connect(str(path), read_only=True, config=options)
            receipt.update(engine_version=duckdb.__version__, settings=bench.measured_settings(db, engine), requested_settings=options, open_mode="read_only=True")
        rating = bench.query_parameters("rating", count, 0)[0]
        for percent in CURSORS:
            for name in WORKLOADS:
                cursor = count * percent // 100
                parameters = [cursor] if name == "page_deep" else [rating, cursor]
                sql = bench.QUERY_SQL[name]
                item = {"workload": name, "cursor_percent": percent, "sql": sql, "parameters": parameters}
                receipt["queries"].append(item)
                if engine == "sqlite":
                    item["plan"] = db.query("EXPLAIN QUERY PLAN " + sql, parameters)[0]
                    rows, item["work"] = db.query(sql, parameters, metrics=True)
                else:
                    item["plan"] = db.execute("EXPLAIN " + sql, parameters).fetchall()
                    rows, item["profile"], item["work"] = duck_query(db, sql, parameters, output / f"{name}-{percent}.profile.json")
                require(bool(item["plan"]), "missing query plan")
                item["returned_rows"] = rows
                item["correctness"] = validate_rows(name, parameters, rows, count)
        receipt["complete"] = True
    except BaseException as error:
        receipt["error"] = f"{type(error).__name__}: {error}"
        raise
    finally:
        try:
            if db is not None:
                db.close()
            if before is not None:
                receipt["source_after"] = source_state(path)
                require(receipt["source_after"] == before, "source changed during diagnostic")
                receipt["source_preserved"] = True
        except BaseException as error:
            receipt["complete"] = False
            receipt["preservation_error"] = f"{type(error).__name__}: {error}"
            raise
        finally:
            with (output / "receipt.json").open("x") as stream:
                json.dump(receipt, stream, indent=2, sort_keys=True)
                stream.write("\n")
    return receipt


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--snapshot", type=Path, required=True)
    parser.add_argument("--engine", choices=("sqlite", "duckdb"), required=True)
    parser.add_argument("--count", type=int, choices=SCALES, required=True)
    parser.add_argument("--memory-mb", type=int, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    run(args.snapshot, args.engine, args.count, args.memory_mb, args.output)


if __name__ == "__main__":
    main()
