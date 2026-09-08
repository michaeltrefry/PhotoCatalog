#!/usr/bin/env python3
"""Versioned synthetic metadata comparison. Never reads private originals or Lightroom data."""

import argparse
import contextlib
import csv
import hashlib
import functools
import tempfile
import json
import math
import os
from pathlib import Path
import platform
import queue
import sqlite3
import subprocess
import sys
import threading
import time

import duckdb
import psutil

VERSION = 2
SEED = 22837
PAGE_SIZE = 200
BASE_DATE = 1262304000
DAY = 86400
COLUMNS = [
    "sequence",
    "id",
    "location",
    "path_display",
    "fingerprint",
    "state",
    "metadata",
    "preview_hash",
    "error",
    "folder_id",
    "captured_at",
    "rating",
    "camera_id",
    "file_bytes",
]
SCHEMA = [
    """CREATE TABLE assets (sequence BIGINT PRIMARY KEY, id VARCHAR NOT NULL UNIQUE,
       location VARCHAR NOT NULL UNIQUE, path_display VARCHAR NOT NULL, fingerprint VARCHAR,
       state VARCHAR NOT NULL, metadata VARCHAR, preview_hash VARCHAR, error VARCHAR,
       folder_id BIGINT NOT NULL, captured_at BIGINT NOT NULL,
       camera_id INTEGER NOT NULL, file_bytes BIGINT NOT NULL)""",
    """CREATE TABLE annotations(asset_id BIGINT PRIMARY KEY REFERENCES assets(sequence),
       rating INTEGER NOT NULL)""",
    """CREATE TABLE asset_keywords(asset_id BIGINT NOT NULL REFERENCES assets(sequence),
       keyword_id INTEGER NOT NULL, PRIMARY KEY(keyword_id,asset_id))""",
    """CREATE TABLE collection_assets(collection_id INTEGER NOT NULL, asset_id BIGINT NOT NULL
       REFERENCES assets(sequence), PRIMARY KEY(collection_id,asset_id))""",
    """CREATE TABLE edits(asset_id BIGINT PRIMARY KEY REFERENCES assets(sequence),
       revision INTEGER NOT NULL, recipe VARCHAR NOT NULL)""",
    "CREATE TABLE recovery_probe(id INTEGER PRIMARY KEY, value INTEGER NOT NULL)",
]
INDEXES = [
    "CREATE INDEX asset_folder_page ON assets(folder_id,sequence)",
    "CREATE INDEX asset_date_page ON assets(captured_at,sequence)",
    "CREATE INDEX annotation_rating_page ON annotations(rating,asset_id)",
]
SELECT = "a.sequence,a.id,a.folder_id,a.captured_at,r.rating,a.preview_hash"
QUERY_SQL = {
    "page_deep": f"SELECT {SELECT} FROM assets a JOIN annotations r ON r.asset_id=a.sequence WHERE a.sequence>? ORDER BY a.sequence LIMIT 200",
    "folder": f"SELECT {SELECT} FROM assets a JOIN annotations r ON r.asset_id=a.sequence WHERE a.folder_id=? AND a.sequence>? ORDER BY a.sequence LIMIT 200",
    "date": f"SELECT {SELECT} FROM assets a JOIN annotations r ON r.asset_id=a.sequence WHERE a.captured_at>=? AND a.captured_at<? ORDER BY a.captured_at,a.sequence LIMIT 200",
    "rating": f"SELECT {SELECT} FROM assets a JOIN annotations r ON r.asset_id=a.sequence WHERE r.rating=? AND a.sequence>? ORDER BY a.sequence LIMIT 200",
    "keyword": f"""SELECT {SELECT} FROM asset_keywords k JOIN assets a ON a.sequence=k.asset_id JOIN annotations r ON r.asset_id=a.sequence
        WHERE k.keyword_id=? AND k.asset_id>? ORDER BY k.asset_id LIMIT 200""",
    "combined": f"""SELECT {SELECT} FROM assets a JOIN annotations r ON r.asset_id=a.sequence WHERE a.folder_id=? AND r.rating=?
        AND a.captured_at>=? AND a.captured_at<? AND a.sequence>? ORDER BY a.sequence LIMIT 200""",
    "collection": f"""SELECT {SELECT} FROM collection_assets c JOIN assets a ON a.sequence=c.asset_id JOIN annotations r ON r.asset_id=a.sequence
        WHERE c.collection_id=? AND c.asset_id>? ORDER BY c.asset_id LIMIT 200""",
    "aggregate": "SELECT camera_id,rating,count(*),sum(file_bytes) FROM assets a JOIN annotations r ON r.asset_id=a.sequence GROUP BY camera_id,rating ORDER BY camera_id,rating",
}


def dump(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")
    temporary.replace(path)


def mix(value):
    value = (value + SEED + 0x9E3779B97F4A7C15) & ((1 << 64) - 1)
    value = ((value ^ (value >> 30)) * 0xBF58476D1CE4E5B9) & ((1 << 64) - 1)
    value = ((value ^ (value >> 27)) * 0x94D049BB133111EB) & ((1 << 64) - 1)
    return value ^ (value >> 31)


@functools.lru_cache(maxsize=6200)
def iso_date(epoch):
    return time.strftime("%Y-%m-%dT00:00:00Z", time.gmtime(epoch))


def asset_row(sequence):
    value = mix(sequence)
    folder = (value >> 8) % (100 if value % 10 < 8 else 10000)
    date = BASE_DATE + ((sequence // 400 + (value >> 20) % 30) % 6200) * DAY
    rating = 0 if value % 10 < 7 else 1 + (value >> 16) % 5
    camera = 0 if value % 10 < 6 else 1 + (value >> 32) % 19
    uid = f"00000000-0000-4000-8000-{sequence:012x}"
    path = f"/synthetic/volume/{folder:05d}/IMG_{sequence:012d}.CR2"
    digest = hashlib.sha256(str(sequence).encode()).hexdigest()
    dimensions = [(4272, 2848), (6000, 4000), (8256, 5504), (4000, 3000)][camera % 4]
    metadata = json.dumps(
        dict(
            format="CR2"
            if value % 100 < 85
            else "DNG"
            if value % 100 < 90
            else "JPEG"
            if value % 100 < 97
            else "TIFF",
            width=dimensions[0],
            height=dimensions[1],
            orientation=1 + value % 8,
            camera_make=f"Synthetic maker {camera // 5}",
            camera_model=f"Synthetic body {camera}",
            captured_at=iso_date(date),
            preview_source="synthetic metadata; no image decoding",
            iso=100 * (1 + value % 32),
            lens=f"Synthetic lens {value % 40}",
            aperture=(14 + value % 100) / 10,
            source_record=f"fixture-{sequence:012d}",
            description=f"Synthetic photo in folder {folder}; seed {SEED}; record {sequence}",
        ),
        separators=(",", ":"),
    )
    return (
        sequence,
        uid,
        path,
        path,
        digest,
        "ready",
        metadata,
        digest,
        None,
        folder,
        date,
        rating,
        camera,
        12_000_000 + value % 80_000_000,
    )


def keywords(sequence):
    value = mix(sequence)
    return [(sequence, value % 32), (sequence, 32 + (value >> 10) % 4096)]


def collections(sequence):
    return [(mix(sequence) % 100, sequence)] if sequence % 5 == 0 else []


def settings(engine, memory_mb):
    return (
        {
            "journal_mode": "wal",
            "synchronous": 2,
            "fullfsync": 1,
            "foreign_keys": 1,
            "cache_size": -(memory_mb * 1024),
            "mmap_size": 0,
            "temp_store": 1,
            "busy_timeout": 5000,
            "wal_autocheckpoint": 1000,
        }
        if engine == "sqlite"
        else {
            "memory_limit": f"{memory_mb}MiB",
            "threads": 4,
            "checkpoint_threshold": "16MiB",
            "preserve_insertion_order": True,
            "enable_progress_bar": False,
        }
    )


def connect(engine, path, memory_mb=256):
    if engine == "sqlite":
        db = sqlite3.connect(
            path, timeout=5, isolation_level=None, check_same_thread=False
        )
        for key, value in settings(engine, memory_mb).items():
            db.execute(f"PRAGMA {key}={value}")
    else:
        options = settings(engine, memory_mb).copy()
        options.pop("enable_progress_bar")
        db = duckdb.connect(str(path), config=options)
        db.execute("SET enable_progress_bar=false")
    return db


def measured_settings(db, engine):
    if engine == "sqlite":
        return {
            key: db.execute(f"PRAGMA {key}").fetchone()[0]
            for key in settings(engine, 256)
        }
    return dict(
        db.execute(
            "SELECT name,value FROM duckdb_settings() WHERE name IN ('memory_limit','threads','checkpoint_threshold','preserve_insertion_order','enable_progress_bar')"
        ).fetchall()
    )


def create_schema(db, engine):
    for statement in SCHEMA:
        if engine == "sqlite":
            statement = statement.replace(
                "sequence BIGINT PRIMARY KEY", "sequence INTEGER PRIMARY KEY"
            )
        db.execute(statement)
    db.execute("INSERT INTO recovery_probe VALUES(1,0)")
    if engine == "sqlite":
        db.execute("PRAGMA application_id=1346913089")
        db.execute("PRAGMA user_version=1")


def add_indexes(db):
    for statement in INDEXES:
        db.execute(statement)
    db.execute("ANALYZE")


def bulk_insert(db, table, rows):
    if not rows:
        return
    placeholders = "(" + ",".join("?" for _ in rows[0]) + ")"
    db.execute(
        f"INSERT INTO {table} VALUES" + ",".join(placeholders for _ in rows),
        [value for row in rows for value in row],
    )


def insert_batch(db, rows, bulk=False):
    if bulk:
        bulk_insert(db, "assets", [row[:11] + row[12:] for row in rows])
        bulk_insert(db, "annotations", [(row[0], row[11]) for row in rows])
        bulk_insert(
            db, "asset_keywords", [pair for row in rows for pair in keywords(row[0])]
        )
        bulk_insert(
            db,
            "collection_assets",
            [pair for row in rows for pair in collections(row[0])],
        )
        return
    raise ValueError(
        "runtime batches require bulk inserts; CSV loader has its own bounded batches"
    )


def checkpoint(db, engine):
    db.execute(
        "PRAGMA wal_checkpoint(TRUNCATE)" if engine == "sqlite" else "CHECKPOINT"
    )


def size_bytes(path):
    return sum(
        p.stat().st_size
        for p in Path(path).parent.glob(Path(path).name + "*")
        if p.is_file()
    )


class Monitor:
    def __enter__(self):
        self.stop = threading.Event()
        self.peak_rss = psutil.Process().memory_info().rss

        def sample():
            while not self.stop.wait(0.01):
                self.peak_rss = max(self.peak_rss, psutil.Process().memory_info().rss)

        self.thread = threading.Thread(target=sample, daemon=True)
        self.thread.start()
        return self

    def __exit__(self, *_):
        self.stop.set()
        self.thread.join()
        self.peak_rss = max(self.peak_rss, psutil.Process().memory_info().rss)


def generate(folder, count):
    folder = Path(folder)
    folder.mkdir(parents=True, exist_ok=True)
    assert not any(folder.iterdir()), "generator destination must be empty"
    started = time.perf_counter()
    sums = dict(
        count=count,
        sequence_sum=count * (count + 1) // 2,
        rating_sum=0,
        captured_sum=0,
        bytes_sum=0,
        keywords=count * 2,
        collections=count // 5,
    )
    with (
        (folder / "assets.csv").open("w", newline="") as a,
        (folder / "keywords.csv").open("w", newline="") as k,
        (folder / "collections.csv").open("w", newline="") as c,
        (folder / "annotations.csv").open("w", newline="") as r,
    ):
        aw, kw, cw = csv.writer(a), csv.writer(k), csv.writer(c)
        rw = csv.writer(r)
        rw.writerow(["asset_id", "rating"])
        aw.writerow([column for column in COLUMNS if column != "rating"])
        kw.writerow(["asset_id", "keyword_id"])
        cw.writerow(["collection_id", "asset_id"])
        for sequence in range(1, count + 1):
            row = asset_row(sequence)
            aw.writerow(row[:11] + row[12:])
            rw.writerow((row[0], row[11]))
            kw.writerows(keywords(sequence))
            cw.writerows(collections(sequence))
            sums["rating_sum"] += row[11]
            sums["captured_sum"] += row[10]
            sums["bytes_sum"] += row[13]
    digests = {}
    for path in folder.glob("*.csv"):
        with path.open("rb") as file:
            digests[path.name] = hashlib.file_digest(file, "sha256").hexdigest()
    dump(
        folder / "manifest.json",
        dict(
            version=VERSION,
            seed=SEED,
            count=count,
            expected=sums,
            csv_sha256=digests,
            generator_seconds=time.perf_counter() - started,
        ),
    )


def verify(db, manifest):
    count, sequence_sum, rating_sum, captured_sum, bytes_sum = db.execute(
        "SELECT count(*),sum(sequence),sum(rating),sum(captured_at),sum(file_bytes) FROM assets a JOIN annotations r ON r.asset_id=a.sequence"
    ).fetchone()
    actual = dict(
        count=count,
        sequence_sum=sequence_sum,
        rating_sum=rating_sum,
        captured_sum=captured_sum,
        bytes_sum=bytes_sum,
        keywords=db.execute("SELECT count(*) FROM asset_keywords").fetchone()[0],
        collections=db.execute("SELECT count(*) FROM collection_assets").fetchone()[0],
    )
    assert actual == manifest["expected"], (actual, manifest["expected"])
    for sequence in [1, max(1, count // 2), count]:
        row = db.execute(
            "SELECT "
            + ",".join("r.rating" if c == "rating" else "a." + c for c in COLUMNS)
            + " FROM assets a JOIN annotations r ON r.asset_id=a.sequence WHERE sequence=?",
            [sequence],
        ).fetchone()
        assert tuple(row) == asset_row(sequence), (sequence, row)
    return actual


def load(engine, path, source, memory_mb):
    path, source = Path(path), Path(source)
    assert not path.exists(), "database destination must not exist"
    path.parent.mkdir(parents=True, exist_ok=True)
    manifest = json.loads((source / "manifest.json").read_text())
    started = time.perf_counter()
    with Monitor() as monitor:
        db = connect(engine, path, memory_mb)
        create_schema(db, engine)
        if engine == "duckdb":
            for table, filename in [
                ("assets", "assets.csv"),
                ("annotations", "annotations.csv"),
                ("asset_keywords", "keywords.csv"),
                ("collection_assets", "collections.csv"),
            ]:
                escaped = str(source / filename).replace("'", "''")
                db.execute("BEGIN")
                db.execute(
                    f"COPY {table} FROM '{escaped}' (HEADER true, DELIMITER ',', NULL '')"
                )
                db.execute("COMMIT")
        else:
            for table, filename in [
                ("assets", "assets.csv"),
                ("annotations", "annotations.csv"),
                ("asset_keywords", "keywords.csv"),
                ("collection_assets", "collections.csv"),
            ]:
                with (source / filename).open(newline="") as file:
                    reader = csv.reader(file)
                    header = next(reader)
                    batch = []
                    sql = (
                        f"INSERT INTO {table} VALUES("
                        + ",".join("?" for _ in header)
                        + ")"
                    )
                    for raw in reader:
                        row = [
                            int(value)
                            if table != "assets" or i in [0, 9, 10, 11, 12]
                            else None
                            if i == 8
                            else value
                            for i, value in enumerate(raw)
                        ]
                        batch.append(row)
                        if len(batch) == 10000:
                            db.execute("BEGIN")
                            db.executemany(sql, batch)
                            db.execute("COMMIT")
                            batch = []
                    if batch:
                        db.execute("BEGIN")
                        db.executemany(sql, batch)
                        db.execute("COMMIT")
        add_indexes(db)
        proof = verify(db, manifest)
        checkpoint(db, engine)
        measured = measured_settings(db, engine)
        db.close()
    return dict(
        engine=engine,
        engine_version=sqlite3.sqlite_version
        if engine == "sqlite"
        else duckdb.__version__,
        count=manifest["count"],
        proof=proof,
        settings=measured,
        load_seconds=time.perf_counter() - started,
        load_peak_rss_bytes=monitor.peak_rss,
        disk_bytes=size_bytes(path),
        ingestion_adapter="Python sqlite3 batches of10000"
        if engine == "sqlite"
        else "native DuckDB COPY per CSV table",
    )


def query_parameters(name, count, iteration):
    # Deep cursors span 50%-95% rather than timing only the beginning or empty tail.
    cursor = int(count * (0.5 + 0.45 * (iteration % 10) / 10))
    value = mix(iteration + SEED)
    if name == "page_deep":
        return [cursor]
    if name == "folder":
        return [value % 100, cursor]
    if name == "date":
        start = asset_row(cursor)[10]
        return [start, start + 365 * DAY]
    if name == "rating":
        return [1 + value % 5, cursor]
    if name == "keyword":
        return [value % 32, cursor]
    if name == "collection":
        return [value % 100, int(count * (0.5 + 0.3 * (iteration % 10) / 10))]
    if name == "combined":
        date = asset_row(cursor)[10]
        return [value % 100, 0, date - 30 * DAY, date + 365 * DAY, cursor]
    return []


def percentile(values, fraction):
    values = sorted(values)
    position = (len(values) - 1) * fraction
    low = int(position)
    high = math.ceil(position)
    return values[low] + (values[high] - values[low]) * (position - low)


def distribution(values):
    return dict(
        n=len(values),
        p50_ms=percentile(values, 0.5),
        p95_ms=percentile(values, 0.95),
        p99_ms=percentile(values, 0.99),
        max_ms=max(values),
        samples_ms=values,
    )


def validate_page(name, parameters, rows, count=None):
    if name == "aggregate":
        return
    assert len(rows) <= PAGE_SIZE
    if count is not None and count >= 1_000_000:
        assert len(rows) == PAGE_SIZE, (
            f"{name}: underfilled {len(rows)}-row page cannot establish the200-row budget"
        )
    for row in rows:
        sequence, uid, folder, date, rating, _ = row
        assert uid == f"00000000-0000-4000-8000-{sequence:012x}"
        if name == "page_deep":
            assert sequence > parameters[0]
        elif name == "folder":
            assert folder == parameters[0] and sequence > parameters[1]
        elif name == "date":
            assert parameters[0] <= date < parameters[1]
        elif name == "rating":
            assert rating == parameters[0] and sequence > parameters[1]
        elif name == "keyword":
            assert (sequence, parameters[0]) in keywords(
                sequence
            ) and sequence > parameters[1]
        elif name == "collection":
            assert (parameters[0], sequence) in collections(
                sequence
            ) and sequence > parameters[1]
        elif name == "combined":
            assert (
                folder == parameters[0]
                and rating == parameters[1]
                and parameters[2] <= date < parameters[3]
                and sequence > parameters[4]
            )
    keys = [(r[3], r[0]) if name == "date" else r[0] for r in rows]
    assert keys == sorted(set(keys)), f"{name}: unstable or duplicate page order"
    if name == "page_deep" and rows:
        assert [r[0] for r in rows] == list(
            range(parameters[0] + 1, parameters[0] + len(rows) + 1)
        )


def timed_query(db, name, parameters, count=None):
    started = time.perf_counter_ns()
    rows = db.execute(QUERY_SQL[name], parameters).fetchall()
    elapsed = (time.perf_counter_ns() - started) / 1e6
    validate_page(name, parameters, rows, count)
    digest = hashlib.sha256(
        json.dumps(rows, separators=(",", ":")).encode()
    ).hexdigest()
    return elapsed, len(rows), digest


def read_workload(
    engine, path, count, memory_mb, repetitions, single=None, iteration=0
):
    started = time.perf_counter_ns()
    with Monitor() as monitor:
        db = connect(engine, path, memory_mb)
        open_ms = (time.perf_counter_ns() - started) / 1e6
        output = {}
        names = [single] if single else QUERY_SQL
        for name in names:
            if not single:
                for warmup in range(3):
                    timed_query(db, name, query_parameters(name, count, warmup))
            samples, correctness = [], []
            for index in range(repetitions):
                i = iteration if single else index
                ms, rows, digest = timed_query(
                    db, name, query_parameters(name, count, i), count
                )
                samples.append(ms)
                correctness.append(dict(iteration=i, rows=rows, sha256=digest))
            output[name] = dict(
                distribution=distribution(samples), correctness=correctness
            )
        measured = measured_settings(db, engine)
        db.close()
    return dict(
        engine=engine,
        count=count,
        open_ms=open_ms,
        peak_rss_bytes=monitor.peak_rss,
        settings=measured,
        workloads=output,
        cache_state="fresh process; OS cache uncontrolled"
        if single
        else "3 warmups per workload",
    )


def plans(engine, path, count, memory_mb):
    db = connect(engine, path, memory_mb)
    result = {}
    for name, sql in QUERY_SQL.items():
        result[name] = db.execute(
            ("EXPLAIN QUERY PLAN " if engine == "sqlite" else "EXPLAIN ") + sql,
            query_parameters(name, count, 0),
        ).fetchall()
    db.close()
    return result


def mixed(engine, path, count, memory_mb, repetitions):
    """Two live connections; foreground operations never share background transaction locks."""
    db = connect(engine, path, memory_mb)
    start = threading.Event()
    stop = threading.Event()
    active = threading.Event()
    background = dict(batches=0, rows=0, errors=[], samples_ms=[])
    disk_before = size_bytes(path)

    def importer():
        connection = connect(engine, path, memory_mb)
        sequence = count + 1
        start.set()
        try:
            while not stop.is_set():
                # Materialization is outside the transaction; both engines receive identical 32-row batches.
                batch = [asset_row(i) for i in range(sequence, sequence + 32)]
                began = time.perf_counter_ns()
                connection.execute("BEGIN")
                active.set()
                insert_batch(connection, batch, bulk=True)
                connection.execute("COMMIT")
                active.clear()
                background["samples_ms"].append((time.perf_counter_ns() - began) / 1e6)
                background["batches"] += 1
                background["rows"] += len(batch)
                sequence += len(batch)
                # Foreground-preference yield is identical for both engines and bounds producer pressure.
                stop.wait(0.002)
        except Exception as error:
            background["errors"].append(repr(error))
            active.clear()
        finally:
            connection.close()

    with Monitor() as monitor:
        thread = threading.Thread(target=importer)
        thread.start()
        start.wait(10)
        samples = {"rating": [], "edit": [], "page_during_import": []}
        overlap = 0
        errors = []
        try:
            for i in range(repetitions):
                # Wait for an actual active import transaction, not merely a living idle thread.
                if not active.wait(10):
                    raise RuntimeError("background import did not become active")
                overlap += 1
                sequence = 1 + (mix(i) % count)
                operation = "rating" if i % 2 == 0 else "edit"
                began = time.perf_counter_ns()
                db.execute("BEGIN")
                if operation == "rating":
                    db.execute(
                        "UPDATE annotations SET rating=? WHERE asset_id=?",
                        [1 + i % 5, sequence],
                    )
                else:
                    db.execute(
                        "INSERT INTO edits VALUES(?,?,?) ON CONFLICT(asset_id) DO UPDATE SET revision=excluded.revision,recipe=excluded.recipe",
                        [
                            sequence,
                            i,
                            json.dumps({"version": 1, "exposure": (i % 9 - 4) / 10}),
                        ],
                    )
                db.execute("COMMIT")
                samples[operation].append((time.perf_counter_ns() - began) / 1e6)
                if operation == "rating":
                    assert (
                        db.execute(
                            "SELECT rating FROM annotations WHERE asset_id=?",
                            [sequence],
                        ).fetchone()[0]
                        == 1 + i % 5
                    )
                else:
                    assert (
                        db.execute(
                            "SELECT revision FROM edits WHERE asset_id=?", [sequence]
                        ).fetchone()[0]
                        == i
                    )
                ms, _, _ = timed_query(
                    db, "page_deep", query_parameters("page_deep", count, i)
                )
                samples["page_during_import"].append(ms)
        except Exception as error:
            errors.append(repr(error))
            with contextlib.suppress(Exception):
                db.execute("ROLLBACK")
        finally:
            stop.set()
            thread.join(30)
            assert not thread.is_alive(), "background importer did not stop"
        after_count = db.execute("SELECT count(*) FROM assets").fetchone()[0]
        assert after_count == count + background["rows"], (after_count, background)
        checkpoint(db, engine)
        db.close()
    return dict(
        workloads={key: distribution(value) for key, value in samples.items() if value},
        concurrent_starts=overlap,
        background=background,
        errors=errors,
        peak_rss_bytes=monitor.peak_rss,
        disk_before=disk_before,
        disk_after=size_bytes(path),
    )


def crash_worker(engine, path, memory_mb, stage):
    db = connect(engine, path, memory_mb)
    db.execute("BEGIN")
    db.execute("UPDATE recovery_probe SET value=1 WHERE id=1")
    db.execute("COMMIT")
    db.execute("BEGIN")
    db.execute("UPDATE recovery_probe SET value=2 WHERE id=1")
    if stage == "after":
        db.execute("COMMIT")
    # No connection close/checkpoint/destructors: parent forcefully kills this process at the observed boundary.
    print("CRASH_READY", flush=True)
    time.sleep(300)


def recovery(engine, path, memory_mb):
    result = {}
    for stage, expected in [("before", 1), ("after", 2)]:
        process = subprocess.Popen(
            [
                sys.executable,
                __file__,
                "crash-worker",
                "--engine",
                engine,
                "--db",
                str(path),
                "--memory-mb",
                str(memory_mb),
                "--stage",
                stage,
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        marker_queue = queue.Queue()
        threading.Thread(
            target=lambda: marker_queue.put(process.stdout.readline().strip()),
            daemon=True,
        ).start()
        try:
            marker = marker_queue.get(timeout=60)
            if marker != "CRASH_READY":
                _, error = process.communicate(timeout=30)
                raise RuntimeError(error or marker)
        finally:
            if process.poll() is None:
                process.kill()
            process.wait(timeout=30)
            process.stdout.close()
            process.stderr.close()
        db = connect(engine, path, memory_mb)
        actual = db.execute("SELECT value FROM recovery_probe WHERE id=1").fetchone()[0]
        assert actual == expected, (stage, actual, expected)
        result[stage] = dict(
            expected=expected, actual=actual, forced_exit_code=process.returncode
        )
        db.close()
    return result


def child(arguments):
    process = subprocess.run(
        [sys.executable, __file__] + list(map(str, arguments)),
        text=True,
        capture_output=True,
    )
    if process.returncode:
        return dict(error=process.stderr, returncode=process.returncode)
    return json.loads(process.stdout)


def host():
    return dict(
        platform=platform.platform(),
        machine=platform.machine(),
        python=sys.version,
        cpu_count=psutil.cpu_count(),
        ram_bytes=psutil.virtual_memory().total,
        available_ram_bytes=psutil.virtual_memory().available,
        load_average=os.getloadavg() if hasattr(os, "getloadavg") else None,
        executable=sys.executable,
        duckdb_version=duckdb.__version__,
        sqlite_version=sqlite3.sqlite_version,
    )


def campaign(
    root,
    counts,
    repetitions,
    fresh_repetitions,
    native_probe=None,
    prepare_only=False,
    resume_prepared=False,
):
    root = Path(root)
    root.mkdir(parents=True, exist_ok=True)
    script_sha = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    if resume_prepared:
        manifest = json.loads((root / "campaign.json").read_text())
        assert manifest.get("prepared_only") and not manifest.get("complete"), (
            "not an unused prepared campaign"
        )
        assert (
            manifest["script_sha256"] == script_sha and manifest["counts"] == counts
        ), "prepared campaign contract changed"
        assert (
            manifest["repetitions"] == repetitions
            and manifest["fresh_repetitions"] == fresh_repetitions
        )
        manifest["measurement_host"] = host()
    else:
        assert not any(root.iterdir()), (
            "campaign root must be empty; never overwrite evidence"
        )
        manifest = dict(
            version=VERSION,
            seed=SEED,
            host=host(),
            script_sha256=script_sha,
            counts=counts,
            repetitions=repetitions,
            fresh_repetitions=fresh_repetitions,
            cold_os_cache="NOT MEASURED: fresh processes do not evict OS caches",
            results={},
        )
    dump(root / "campaign.json", manifest)
    for count in counts:
        source = root / str(count) / "csv"
        if not resume_prepared:
            generate(source, count)
        by_engine = {}
        for engine in ["sqlite", "duckdb"]:
            path = (
                root
                / str(count)
                / engine
                / ("catalog.sqlite3" if engine == "sqlite" else "catalog.duckdb")
            )
            common = ["--engine", engine, "--db", path, "--memory-mb", 256]
            if resume_prepared:
                result = json.loads((root / str(count) / f"{engine}.json").read_text())
            else:
                result = dict(
                    load=child(
                        [
                            "load",
                            "--engine",
                            engine,
                            "--db",
                            path,
                            "--memory-mb",
                            4096,
                            "--source",
                            source,
                        ]
                    )
                )
            if prepare_only:
                by_engine[engine] = result
                dump(root / str(count) / f"{engine}.json", result)
                continue
            if "error" in result["load"]:
                by_engine[engine] = result
                dump(root / str(count) / f"{engine}.json", result)
                continue
            result["plans"] = child(["plans", *common, "--count", count])
            for memory in [256, 64]:
                reads = [
                    "--engine",
                    engine,
                    "--db",
                    path,
                    "--memory-mb",
                    memory,
                    "--count",
                    count,
                ]
                result[f"warm_{memory}mb"] = child(
                    ["read", *reads, "--repetitions", repetitions]
                )
            fresh = {}
            for name in QUERY_SQL:
                if name == "aggregate":
                    continue
                raw = [
                    child(
                        [
                            "read",
                            *common,
                            "--count",
                            count,
                            "--single",
                            name,
                            "--iteration",
                            i,
                            "--repetitions",
                            1,
                        ]
                    )
                    for i in range(fresh_repetitions)
                ]
                successful = [r for r in raw if "error" not in r]
                fresh[name] = dict(errors=[r for r in raw if "error" in r], raw=raw)
                if successful:
                    fresh[name].update(
                        open_plus_query=distribution(
                            [
                                r["open_ms"]
                                + r["workloads"][name]["distribution"]["samples_ms"][0]
                                for r in successful
                            ]
                        ),
                        query_only=distribution(
                            [
                                r["workloads"][name]["distribution"]["samples_ms"][0]
                                for r in successful
                            ]
                        ),
                        peak_rss_bytes=max(r["peak_rss_bytes"] for r in successful),
                    )
            result["fresh_process"] = fresh
            result["mixed"] = child(
                ["mixed", *common, "--count", count, "--repetitions", repetitions * 2]
            )
            result["recovery"] = child(["recovery", *common])
            by_engine[engine] = result
            dump(root / str(count) / f"{engine}.json", result)
            print(f"{engine} {count}: completed", file=sys.stderr, flush=True)
        # Identical reads before mixed writes must return identical results, not merely similar latency.
        for memory in [256, 64]:
            for name in QUERY_SQL:
                left = by_engine["sqlite"].get(f"warm_{memory}mb", {})
                right = by_engine["duckdb"].get(f"warm_{memory}mb", {})
                if "workloads" in left and "workloads" in right:
                    assert (
                        left["workloads"][name]["correctness"]
                        == right["workloads"][name]["correctness"]
                    ), (count, memory, name)
        if native_probe and not prepare_only:
            command = [
                str(native_probe),
                "--catalog",
                str(root / str(count) / "sqlite"),
                "--count",
                str(count),
                "--repetitions",
                str(repetitions),
            ]
            with tempfile.TemporaryFile(mode="w+t") as captured:
                process = subprocess.Popen(
                    command, stdout=captured, stderr=subprocess.PIPE, text=True
                )
                peak_rss = 0
                while process.poll() is None:
                    with contextlib.suppress(psutil.NoSuchProcess):
                        peak_rss = max(
                            peak_rss, psutil.Process(process.pid).memory_info().rss
                        )
                    time.sleep(0.001)
                _, error = process.communicate()
                assert process.returncode == 0, error
                captured.seek(0)
                output = captured.read()
            native = json.loads(output)
            native["sampled_peak_rss_bytes"] = peak_rss
            dump(root / str(count) / "rust_native.json", native)
        manifest["results"][str(count)] = {
            engine: f"{count}/{engine}.json" for engine in by_engine
        }
        dump(root / "campaign.json", manifest)
    manifest["complete"] = not prepare_only
    manifest["prepared_only"] = prepare_only
    manifest["host_after"] = host()
    dump(root / "campaign.json", manifest)
    if not prepare_only:
        dump(root / "budget_summary.json", summarize(root))
    return manifest


def summarize(root):
    root = Path(root)
    manifest = json.loads((root / "campaign.json").read_text())
    summary = {}
    for count in manifest["counts"]:
        summary[str(count)] = {}
        for engine in ["sqlite", "duckdb"]:
            result = json.loads((root / str(count) / f"{engine}.json").read_text())
            checks = {}
            pages = {}
            for memory in [256, 64]:
                warm = result.get(f"warm_{memory}mb", {})
                checks[f"warm_{memory}mb"] = set(warm.get("workloads", {})) == set(
                    QUERY_SQL
                ) and all(
                    v["distribution"]["p95_ms"] <= 100
                    and v["distribution"].get("n") == manifest["repetitions"]
                    for k, v in warm.get("workloads", {}).items()
                    if k != "aggregate"
                )
                checks[f"rss_{memory}mb"] = (
                    warm.get("peak_rss_bytes", float("inf")) <= 4 * 1024**3
                )
                pages[f"warm_{memory}mb"] = {
                    name: value["distribution"]["p95_ms"]
                    for name, value in warm.get("workloads", {}).items()
                }
            fresh = result.get("fresh_process", {})
            checks["fresh_process"] = set(fresh) == set(QUERY_SQL) - {
                "aggregate"
            } and all(
                not value["errors"]
                and value.get("open_plus_query", {}).get("n")
                == manifest["fresh_repetitions"]
                and value.get("open_plus_query", {}).get("p95_ms", float("inf")) <= 500
                for value in fresh.values()
            )
            checks["fresh_rss"] = set(fresh) == set(QUERY_SQL) - {"aggregate"} and all(
                value.get("peak_rss_bytes", float("inf")) <= 4 * 1024**3
                for value in fresh.values()
            )
            query_plans = result.get("plans", {})
            checks["query_plans"] = set(query_plans) == set(QUERY_SQL) and all(
                isinstance(query_plans[name], list) and bool(query_plans[name])
                for name in QUERY_SQL
            )
            mixed = result.get("mixed", {})
            checks["mixed_writes"] = (
                not mixed.get("error")
                and not mixed.get("errors")
                and not mixed.get("background", {}).get("errors")
                and all(
                    mixed.get("workloads", {}).get(name, {}).get("p95_ms", float("inf"))
                    <= 100
                    and mixed.get("workloads", {}).get(name, {}).get("n")
                    == manifest["repetitions"]
                    for name in ["rating", "edit"]
                )
            )
            checks["recovery"] = all(
                result.get("recovery", {}).get(stage, {}).get("actual") == expected
                for stage, expected in [("before", 1), ("after", 2)]
            )
            checks["load_integrity"] = "error" not in result["load"]
            summary[str(count)][engine] = dict(
                checks=checks, all_pass=all(checks.values()), p95_ms=pages
            )
        native_path = root / str(count) / "rust_native.json"
        if native_path.exists():
            native = json.loads(native_path.read_text())
            summary[str(count)]["rust_native"] = dict(
                all_pass=native["p95_ms"] <= 100
                and native["sampled_peak_rss_bytes"] <= 4 * 1024**3
                and all(
                    native["native_mixed"][name]["p95_ms"] <= 100
                    for name in ["rating", "edit"]
                ),
                page_p95_ms=native["p95_ms"],
                rating_p95_ms=native["native_mixed"]["rating"]["p95_ms"],
                edit_p95_ms=native["native_mixed"]["edit"]["p95_ms"],
            )
    return dict(
        scale_results=summary,
        scope="Database page/write budgets only; no UI/preview/RAW performance claim",
        fixed_budgets=dict(
            warm_p95_ms=100,
            fresh_process_p95_ms=500,
            write_p95_ms=100,
            browse_rss_bytes=4 * 1024**3,
        ),
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "command",
        choices=[
            "generate",
            "load",
            "read",
            "plans",
            "mixed",
            "recovery",
            "crash-worker",
            "campaign",
        ],
    )
    parser.add_argument("--engine", choices=["sqlite", "duckdb"])
    parser.add_argument("--db", type=Path)
    parser.add_argument("--source", type=Path)
    parser.add_argument("--root", type=Path)
    parser.add_argument("--native-probe", type=Path)
    phase = parser.add_mutually_exclusive_group()
    phase.add_argument("--prepare-only", action="store_true")
    phase.add_argument("--resume-prepared", action="store_true")
    parser.add_argument("--count", type=int, default=10000)
    parser.add_argument("--counts", default="1000000,5000000,10000000")
    parser.add_argument("--memory-mb", type=int, default=256)
    parser.add_argument("--repetitions", type=int, default=100)
    parser.add_argument("--fresh-repetitions", type=int, default=20)
    parser.add_argument("--single", choices=QUERY_SQL)
    parser.add_argument("--iteration", type=int, default=0)
    parser.add_argument("--stage", choices=["before", "after"])
    args = parser.parse_args()
    assert (
        args.count > 0
        and args.memory_mb > 0
        and args.repetitions > 0
        and args.fresh_repetitions > 0
    )
    if args.native_probe:
        args.native_probe = args.native_probe.resolve()
    if args.command == "generate":
        result = generate(args.source, args.count)
    elif args.command == "load":
        result = load(args.engine, args.db, args.source, args.memory_mb)
    elif args.command == "read":
        result = read_workload(
            args.engine,
            args.db,
            args.count,
            args.memory_mb,
            args.repetitions,
            args.single,
            args.iteration,
        )
    elif args.command == "plans":
        result = plans(args.engine, args.db, args.count, args.memory_mb)
    elif args.command == "mixed":
        result = mixed(
            args.engine, args.db, args.count, args.memory_mb, args.repetitions
        )
    elif args.command == "recovery":
        result = recovery(args.engine, args.db, args.memory_mb)
    elif args.command == "crash-worker":
        return crash_worker(args.engine, args.db, args.memory_mb, args.stage)
    else:
        result = campaign(
            args.root,
            [int(n) for n in args.counts.split(",")],
            args.repetitions,
            args.fresh_repetitions,
            args.native_probe,
            args.prepare_only,
            args.resume_prepared,
        )
    print(json.dumps(result))


if __name__ == "__main__":
    main()
