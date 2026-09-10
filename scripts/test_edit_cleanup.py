import os
from pathlib import Path
import tempfile
import unittest
import edit_cleanup as cleanup


class DisposableCleanupAdmission(unittest.TestCase):
    def test_tree_refuses_symlink_and_preserves_outside_source(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder).resolve();catalog=root/'catalog';catalog.mkdir()
            source=root/'source';source.write_bytes(b'original')
            try:(catalog/'foreign').symlink_to(source)
            except OSError as exc:self.skipTest(str(exc))
            with self.assertRaises(ValueError):cleanup.tree(root,[catalog],1024,4096)
            self.assertEqual(source.read_bytes(),b'original')

    def test_tree_records_hard_links_as_distinct_paths_without_copying_bytes(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder).resolve();catalog=root/'catalog';catalog.mkdir()
            source=catalog/'payload';source.write_bytes(b'encoded')
            os.link(source,catalog/'second')
            files,directories=cleanup.tree(root,[catalog],1024,4096)
            self.assertEqual(len(files),2)
            self.assertEqual(files[0]['sha256'],files[1]['sha256'])
            self.assertEqual(files[0]['inode'],files[1]['inode'])
            self.assertEqual(directories,[str(catalog)])

    def test_tree_enforces_declared_logical_bytes_before_deletion(self):
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder).resolve();catalog=root/'catalog';catalog.mkdir()
            path=catalog/'payload';path.write_bytes(b'12345678')
            with self.assertRaises(ValueError):cleanup.tree(root,[catalog],1024,7)
            self.assertTrue(path.exists())

if __name__=='__main__':unittest.main()
