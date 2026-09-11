"""Bounded page-lifetime and canonical-replay regressions; no native or source files."""
import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import unittest
import weakref

SPEC = importlib.util.spec_from_file_location("page_memory_runner", Path(__file__).parents[1]/"scripts/run_lightroom_inspection.py")
RUN = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUN)


def runner(require):
    value = RUN.Runner.__new__(RUN.Runner)
    value.config = copy.deepcopy(RUN.DEFAULTS)
    value.require = require
    return value


class Page(list):
    pass


class Row(dict):
    pass


class PageMemoryTests(unittest.TestCase):
    def test_entire_previous_page_and_last_row_are_freed_before_next_load(self):
        references = []
        def require(key, arguments):
            self.assertTrue(all(reference() is None for reference in references))
            index = key[-1]
            page = Page()
            if index < 3:
                row = Row(sequence=index+1, state="available", evidence=Row(metadata=Row(bytes=123)))
                page.append(row)
                references.extend([weakref.ref(row), weakref.ref(row["evidence"]), weakref.ref(row["evidence"]["metadata"])])
            references.append(weakref.ref(page))
            return {"value": page, "record": {"sequence": index+10}}
        result = runner(require).pages("plan", "r", "paths", "scope")
        self.assertEqual(result["rows"], 3)
        self.assertEqual(result["available_reference_bytes"], 369)
        self.assertEqual(result["pages"], [10, 11, 12])
        self.assertTrue(all(reference() is None for reference in references))

    def test_streamed_canonical_bytes_match_original_for_typed_nested_values(self):
        cases = [None, True, False, 0, -1, 2**63-1, -0.0, 1e-300, 1.2345678901234567,
                 float("inf"), float("-inf"), float("nan"),
                 {"z": [None, {"quotes": '\\"\n\t\x00', "unicode": "雪😀\ud800\udfff"}], "a": [1, 2]},
                 {"cells": [{"type": "Blob", "value": "00ff807f"*32768}, {"Blob": list(range(256))*256}, {"Text": list(b"opaque\xff\x00")},
                            {"Real": 0x8000000000000000}, {"Integer": -9223372036854775808}]}]
        for value in cases:
            with self.subTest(kind=type(value).__name__):
                class Sink:
                    def __init__(self): self.chunks = []
                    def update(self, chunk): self.chunks.append(chunk)
                sink = Sink()
                RUN.update_canonical_hash(sink, value)
                self.assertEqual(b"".join(sink.chunks), RUN.encoded(value))
                digest = hashlib.sha256()
                RUN.update_canonical_hash(digest, value)
                self.assertEqual(digest.hexdigest(), hashlib.sha256(RUN.encoded(value)).hexdigest())

    def test_single_large_escaped_string_has_bounded_byte_chunks(self):
        value = {"text": "😀\ud800\n"*40000}
        class Sink:
            def __init__(self): self.digest = hashlib.sha256(); self.largest = 0; self.total = 0
            def update(self, chunk):
                self.largest = max(self.largest, len(chunk)); self.total += len(chunk)
                self.digest.update(chunk)
        sink = Sink()
        RUN.update_canonical_hash(sink, value)
        expected = RUN.encoded(value)
        self.assertLessEqual(sink.largest, 65536)
        self.assertEqual(sink.total, len(expected))
        self.assertEqual(sink.digest.hexdigest(), hashlib.sha256(expected).hexdigest())

    def test_interrupted_restart_replays_full_prefix_with_exact_digest_and_sparse_cursor(self):
        source = [[{"sequence": n, "source_id": f"s{n}", "revision_id": "r", "table": "opaque",
                    "cells": [{"Text": [0, 255, 34]}, {"Real": "8000000000000000"}]} for n in [3, 17]],
                  [{"sequence": 205, "source_id": "s205", "revision_id": "r", "table": "other"}], []]
        saved = {}; requests = []; interrupted = True
        def require(key, arguments):
            nonlocal interrupted
            index = key[-1]; requests.append(arguments[4])
            if index == 1 and interrupted:
                interrupted = False
                raise RUN.PauseRequested("fixture boundary")
            if index not in saved:
                saved[index] = json.dumps({"value": source[index], "record": {"sequence": index+1}})
            return json.loads(saved[index])
        with self.assertRaises(RUN.PauseRequested):
            runner(require).pages("plan", "r", "rows", "scope")
        self.assertEqual(list(saved), [0])
        result = runner(require).pages("plan", "r", "rows", "scope")
        self.assertEqual(requests, [0, 17, 0, 17, 205])
        self.assertEqual(list(saved), [0, 1, 2])
        self.assertEqual(result["rows"], 3)
        self.assertEqual(result["last_sequence"], 205)
        self.assertEqual(result["counts"], {"opaque": 2, "other": 1})
        self.assertEqual(result["pages"], [1, 2])
        rows = source[0]+source[1]
        self.assertEqual(result["content_sha256"], hashlib.sha256(b"".join(RUN.encoded(row) for row in rows)).hexdigest())
        self.assertEqual(result["identity_sha256"], hashlib.sha256(b"".join(RUN.encoded([row["sequence"], row["source_id"], "r", row["table"]]) for row in rows)).hexdigest())

    def test_bad_page_identity_or_cursor_never_publishes_summary(self):
        good = {"sequence": 10, "source_id": "s", "revision_id": "r", "table": "t"}
        cases = [(None, "not an array"), ([good, dict(good)], "non-increasing"),
                 ([dict(good, revision_id="wrong")], "identity mismatch")]
        for value, message in cases:
            with self.subTest(message=message):
                with self.assertRaisesRegex(ValueError, message):
                    runner(lambda key, arguments: {"value": value, "record": {"sequence": 1}}).pages("plan", "r", "rows", "scope")


if __name__ == "__main__":
    unittest.main()
