"""JSON input compatibility and release before object-tree construction."""
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("json_memory_runner", Path(__file__).parents[1]/"scripts/run_lightroom_inspection.py")
RUN = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUN)


class JsonMemoryTests(unittest.TestCase):
    def compare(self, raw):
        def outcome(function):
            try:
                return ("value", json.dumps(function(), sort_keys=True))
            except (ValueError, UnicodeError) as error:
                return (type(error), error.args, getattr(error, "doc", None),
                        getattr(error, "pos", None), getattr(error, "lineno", None),
                        getattr(error, "colno", None))
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/"input.json"
            path.write_bytes(raw)
            self.assertEqual(outcome(lambda: json.loads(raw)),
                             outcome(lambda: RUN.read_json(path, len(raw))))

    def test_encodings_boms_and_surrogates_keep_byte_input_semantics(self):
        text = '{"text":"雪😀\\ud800", "value":9007199254740993}'
        for encoding in ["utf-8", "utf-8-sig", "utf-16", "utf-16-le", "utf-16-be",
                         "utf-32", "utf-32-le", "utf-32-be"]:
            with self.subTest(encoding=encoding):
                self.compare(text.encode(encoding))
                self.compare(("\ufeff"+text).encode(encoding))
        self.compare(b'"\xed\xa0\x80"')

    def test_malformed_input_preserves_error_payloads(self):
        for raw in [b"", b"{", b"[] trailing", b"\xef\xbb\xbf\xef\xbb\xbf[]",
                    b"\xff", b"\x00", b"\xff\xfe\x00", b"\xff\xfe\x00\x00\x7b",
                    b'"\x00"', b'"\xed\xa0"']:
            with self.subTest(raw=raw):
                self.compare(raw)

    def test_numeric_and_duplicate_key_behavior(self):
        self.compare(b'{"x":1,"x":2}')
        self.compare(b"[NaN,Infinity,-Infinity,-0.0,5e-324,1.7976931348623157e308,9007199254740993]")

    def test_cap_and_missing_file_errors(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/"input.json"
            with self.assertRaises(FileNotFoundError) as error:
                RUN.read_json(path)
            self.assertEqual(error.exception.filename, str(path))
            path.write_bytes(b"[1]")
            with self.assertRaisesRegex(ValueError, "JSON admission limit exceeded"):
                RUN.read_json(path, 2)
            self.assertEqual(RUN.read_json(path, 3), [1])

    def test_input_buffer_is_released_before_decoder_builds_objects(self):
        released = []
        class Buffer(bytes):
            def __del__(self):
                released.append(True)
        class Input:
            read_once = False
            def __enter__(self): return self
            def __exit__(self, *args): return False
            def read(self, count):
                if self.read_once: return b""
                self.read_once = True
                return Buffer(b'{"x":1}')
        decode = json.JSONDecoder.decode
        def checked(decoder, text):
            self.assertEqual(released, [True])
            return decode(decoder, text)
        with patch.object(RUN, "open", return_value=Input(), create=True), patch.object(json.JSONDecoder, "decode", checked):
            self.assertEqual(RUN.read_json("fixture"), {"x": 1})

    def test_near_page_limit_string(self):
        self.compare(b'"'+b"x"*(8*1024**2-2)+b'"')

    def test_admitted_caps_bound_read_requests_and_consume_short_reads(self):
        raw = b'{"value":42}'
        for cap in [len(raw), 8*1024**2, 16*1024**2, 64*1024**2]:
            requests = []
            class Input(io.BytesIO):
                def read(self, count):
                    requests.append(count)
                    return super().read(min(count, 3))
            with self.subTest(cap=cap), patch.object(RUN, "open", return_value=Input(raw), create=True):
                self.assertEqual(RUN.read_json("fixture", cap), {"value":42})
                self.assertLessEqual(max(requests), 65536)

    def test_cap_boundary_stops_before_reading_the_remaining_file(self):
        requests = []
        class Input(io.BytesIO):
            def read(self, count):
                value = super().read(count)
                requests.append(len(value))
                return value
        with patch.object(RUN, "open", return_value=Input(b" "*1000), create=True):
            with self.assertRaisesRegex(ValueError, "JSON admission limit exceeded"):
                RUN.read_json("fixture", 12)
        self.assertEqual(sum(requests), 13)

    def test_unusual_caps_preserve_original_errors(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/"input.json"
            path.write_bytes(b"[1]")
            for cap in [-2, -1, True, 1.5, "invalid", 2**100]:
                def original():
                    with open(path, "rb") as handle: raw = handle.read(cap+1)
                    if len(raw) > cap: raise ValueError(f"JSON admission limit exceeded: {path}")
                    return json.loads(raw)
                def error_of(function):
                    try: return function()
                    except Exception as error: return type(error), error.args
                with self.subTest(cap=cap):
                    self.assertEqual(error_of(original), error_of(lambda: RUN.read_json(path, cap)))


if __name__ == "__main__":
    unittest.main()
