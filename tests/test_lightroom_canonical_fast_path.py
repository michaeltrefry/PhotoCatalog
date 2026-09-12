"""Canonical byte equivalence and bounded fast-path admission; synthetic only."""
import hashlib
import importlib.util
import json
from pathlib import Path
import random
import sys
import tracemalloc
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("canonical_runner", Path(__file__).parents[1]/"scripts/run_lightroom_inspection.py")
RUN = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUN)


def streaming_hash(digest, value):
    """Frozen pre-change algorithm, including per-token byte-copy bound and LF."""
    encoder = json.JSONEncoder(sort_keys=True, separators=(",", ":"), ensure_ascii=True)
    for chunk in encoder.iterencode(value):
        if len(chunk) <= 65536:
            digest.update(chunk.encode())
        else:
            for start in range(0, len(chunk), 65536):
                digest.update(chunk[start:start+65536].encode())
    digest.update(b"\n")


class Sink:
    def __init__(self):
        self.chunks = []
    def update(self, chunk):
        self.chunks.append(chunk)


class CanonicalFastPathTests(unittest.TestCase):
    def equivalent(self, value):
        old, new = Sink(), Sink()
        streaming_hash(old, value)
        RUN.update_canonical_hash(new, value)
        self.assertEqual(b"".join(new.chunks), b"".join(old.chunks))
        self.assertEqual(b"".join(new.chunks), RUN.encoded(value))
        return new

    def test_small_adversarial_values_have_identical_bytes(self):
        for value in [None, True, False, 0, -1, 2**53+1, -(2**63), 2**64-1,
                      -0.0, 5e-324, 1.7976931348623157e308, 1e-7, 1e20,
                      float("nan"), float("inf"), -float("inf"),
                      "", "\x00\x1f\x7f\"\\/\b\f\n\r\t雪😀\ud800\udfff",
                      {"雪": [1, {"z": -0.0, "a": True}], "a": (None, "😀")},
                      {"sequence": 17913, "cells": [{"Real": "8000000000000000"},
                        {"Text": "00ff34"*128}, {"Integer": 17913}]}]:
            with self.subTest(kind=type(value).__name__):
                self.assertIsNotNone(RUN._small_canonical_budget(value))
                self.equivalent(value)

    def test_large_integer_fallback_preserves_conversion_failure_prefix(self):
        previous = sys.get_int_max_str_digits()
        try:
            sys.set_int_max_str_digits(640)
            for item in [{"prefix": 1, "z": 2**2200}, {"integer": 2**1024}]:
                self.assertIsNone(RUN._small_canonical_budget(item))
                outcomes = []
                for function in [streaming_hash, RUN.update_canonical_hash]:
                    sink = Sink()
                    try:
                        function(sink, item)
                        error = None
                    except ValueError as caught:
                        error = str(caught)
                    outcomes.append((b"".join(sink.chunks), error))
                self.assertEqual(outcomes[0], outcomes[1])
                self.assertTrue(outcomes[0][0].startswith(b'{"'))
        finally:
            sys.set_int_max_str_digits(previous)

    def test_conservative_bound_covers_unicode_nodes_and_structure(self):
        rng = random.Random(22844)
        atoms = [None, False, -0.0, 2**63, "😀", "\ud800", "\x7f", "雪", "\\\"\n"]
        def value(depth):
            if depth == 0 or rng.randrange(3) == 0:
                return rng.choice(atoms)
            if rng.randrange(2):
                return [value(depth-1) for _ in range(rng.randrange(6))]
            return {f"{i}😀": value(depth-1) for i in range(rng.randrange(6))}
        for _ in range(200):
            item = value(5)
            bound = RUN._small_canonical_budget(item)
            if bound is not None:
                self.assertLessEqual(len(RUN.encoded(item)), 65536-bound[0])
            self.equivalent(item)

    def test_exact_admission_edges_and_large_fallback(self):
        admitted = "😀"*((65535-2)//12)
        self.assertIsNotNone(RUN._small_canonical_budget(admitted))
        self.assertIsNone(RUN._small_canonical_budget(admitted+"😀"))
        self.assertIsNotNone(RUN._small_canonical_budget([None]*2047))
        self.assertIsNone(RUN._small_canonical_budget([None]*2048))
        nested = 1
        for _ in range(16):
            nested = [nested]
        self.assertIsNotNone(RUN._small_canonical_budget(nested))
        self.assertIsNone(RUN._small_canonical_budget([nested]))
        for item in [admitted+"😀", [None]*2048, [nested], {"blob": "ff"*40000}]:
            with patch.object(json.JSONEncoder, "encode", side_effect=AssertionError("C path must not run")):
                old, new = Sink(), Sink()
                streaming_hash(old, item); RUN.update_canonical_hash(new, item)
                self.assertEqual(b"".join(new.chunks), b"".join(old.chunks))

    def test_admission_never_runs_subclass_hooks_or_converts_unsupported_keys(self):
        class CustomList(list):
            def __iter__(self):
                raise RuntimeError("fixture iterator")
        values = [CustomList([1]), {1: "integer key"}, {"object": object()}]
        cyclic = []; cyclic.append(cyclic); values.append(cyclic)
        for item in values:
            self.assertIsNone(RUN._small_canonical_budget(item))
            outcomes = []
            for function in [streaming_hash, RUN.update_canonical_hash]:
                sink = Sink()
                try:
                    function(sink, item)
                    error = None
                except Exception as caught:
                    error = (type(caught), str(caught))
                outcomes.append((b"".join(sink.chunks), error))
            self.assertEqual(outcomes[0], outcomes[1])

    def test_near_page_limit_preflight_is_constant_space_and_streams(self):
        item = {"value": "\x7f"*(8*1024**2-512)}
        tracemalloc.start()
        try:
            self.assertIsNone(RUN._small_canonical_budget(item))
            _, peak = tracemalloc.get_traced_memory()
        finally:
            tracemalloc.stop()
        self.assertLess(peak, 128*1024)
        old, new = hashlib.sha256(), hashlib.sha256()
        with patch.object(json.JSONEncoder, "encode", side_effect=AssertionError("large C path")):
            streaming_hash(old, item); RUN.update_canonical_hash(new, item)
        self.assertEqual(old.digest(), new.digest())

    def test_mixed_fast_and_fallback_rows_keep_digest_and_lf_boundaries(self):
        rows = [{"sequence": 3, "cells": [1, -0.0]}, {"sequence": 19, "text": "😀"*9000},
                {"sequence": 205, "source_id": "stable", "revision_id": "r", "table": "t"}]
        old, new = hashlib.sha256(), hashlib.sha256()
        for row in rows:
            streaming_hash(old, row); RUN.update_canonical_hash(new, row)
            identity = [row["sequence"], row.get("source_id"), row.get("revision_id"), row.get("table")]
            streaming_hash(old, identity); RUN.update_canonical_hash(new, identity)
        self.assertEqual(old.digest(), new.digest())


if __name__ == "__main__":
    unittest.main()
