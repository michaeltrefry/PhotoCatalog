import base64
import hashlib
import io
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import collect_desktop_notices as c


def archive(files):
    data = io.BytesIO()
    with tarfile.open(fileobj=data, mode='w:gz') as stream:
        for name, payload in files.items():
            entry = tarfile.TarInfo('package/' + name)
            entry.size = len(payload)
            stream.addfile(entry, io.BytesIO(payload))
    return data.getvalue()


class NoticeTests(unittest.TestCase):
    def test_crate_checksum_and_nested_license(self):
        data = archive({'Cargo.toml': b'[package]\nname="test"\n', 'vendor/LICENSE': b'exact\r\nnotice\xff'})
        self.assertEqual(c.unpack_crate(data, c.digest(data))['vendor/LICENSE'], b'exact\r\nnotice\xff')
        with self.assertRaisesRegex(ValueError, 'checksum'):
            c.unpack_crate(data, '0'*64)

    def test_aggregate_preserves_upstream_bytes(self):
        text = b'Copyright\r\n\xff\n'
        combined = c.aggregate([('test@1', {'LICENSE': text})])
        self.assertIn(text, combined)
        self.assertIn(c.digest(text).encode(), combined)

    def test_no_mutable_upstream_revision_or_inferred_owner(self):
        meta = {'name': 'test', 'repository': 'https://github.com/example/test'}
        with patch.object(c, 'download') as fetch:
            self.assertEqual(c.upstream_notices(meta, {'git': {'sha1': 'main'}}, True, {}), ({}, []))
            fetch.assert_not_called()

    def test_npm_integrity_binds_exact_font_and_notice(self):
        files = {'LICENSE': b'font license\n', 'files/font.woff2': b'fontbytes'}
        data = archive(files)
        integrity = 'sha512-' + base64.b64encode(hashlib.sha512(data).digest()).decode()
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root/'files').mkdir()
            for name, value in files.items():
                (root/name).write_bytes(value)
            self.assertEqual(set(c.verify_npm_runtime(data, integrity, root)), set(files))
            (root/'files/font.woff2').write_bytes(b'changed')
            with self.assertRaisesRegex(ValueError, 'differs'):
                c.verify_npm_runtime(data, integrity, root)
            with self.assertRaisesRegex(ValueError, 'integrity mismatch'):
                c.verify_npm_runtime(data + b'x', integrity, root)

    def test_npm_traversal_never_reads_outside(self):
        data = archive({'../LICENSE': b'bad'})
        integrity = 'sha512-' + base64.b64encode(hashlib.sha512(data).digest()).decode()
        with self.assertRaisesRegex(ValueError, 'traversal'):
            c.verify_npm_runtime(data, integrity, Path('/not-read'))


if __name__ == '__main__':
    unittest.main()
