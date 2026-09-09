"""Small semantic checks for experimental queries; no timing assertions."""

import sqlite3
import unittest

import catalog_benchmark as frozen
from query_candidates import CANDIDATE_SQL


class QueryCandidateSemantics(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:")
        self.addCleanup(self.db.close)
        self.db.execute("PRAGMA foreign_keys=ON")
        frozen.create_schema(self.db, "sqlite")
        for statement in frozen.INDEXES:
            self.db.execute(statement)

        # Independent fixtures: discontinuous sequence keys, unrelated dates and
        # folders, nullable previews, and more than 200 initially unannotated assets.
        self.assets = []
        self.annotations = {}
        for index in range(1, 1601):
            sequence = 3 * index + index % 2
            asset = {
                "sequence": sequence,
                "id": f"00000000-0000-4000-8000-{sequence:012x}",
                "folder_id": index % 7,
                "captured_at": 1_000_000 + (index * 47) % 211,
                "preview_hash": None if index % 11 == 0 else f"preview-{index:04x}",
            }
            self.assets.append(asset)
            if index > 225 and index % 13 != 0:
                self.annotations[sequence] = (0, 1, 3, 5)[index % 4]

        # Reverse insertion order keeps insertion order separate from query order.
        for asset in reversed(self.assets):
            sequence = asset["sequence"]
            location = f"/synthetic/candidate/{sequence}.CR2"
            self.db.execute(
                "INSERT INTO assets VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)",
                (sequence, asset["id"], location, location, "fingerprint", "ready",
                 "{}", asset["preview_hash"], None, asset["folder_id"],
                 asset["captured_at"], sequence % 3, sequence * 4096),
            )
        self.db.executemany("INSERT INTO annotations VALUES(?,?)", self.annotations.items())
        self.db.commit()

    def expected(self, name, parameters):
        after = parameters[-1]
        selected = []
        for asset in self.assets:
            sequence = asset["sequence"]
            if sequence <= after or sequence not in self.annotations:
                continue
            rating = self.annotations[sequence]
            if name == "rating" and rating != parameters[0]:
                continue
            selected.append((sequence, asset["id"], asset["folder_id"],
                             asset["captured_at"], rating, asset["preview_hash"]))
        return sorted(selected, key=lambda row: row[0])[:200]

    def compare(self, name, parameters):
        expected = self.expected(name, parameters)
        original = self.db.execute(frozen.QUERY_SQL[name], parameters).fetchall()
        candidate = self.db.execute(CANDIDATE_SQL[name], parameters).fetchall()
        self.assertEqual(original, expected, ("original", name, parameters))
        self.assertEqual(candidate, expected, ("candidate", name, parameters))
        return candidate

    def test_first_200_full_projection_with_gaps_cursors_and_multiple_ratings(self):
        cursors = (0, self.assets[0]["sequence"], self.assets[225]["sequence"] - 1,
                   self.assets[225]["sequence"], self.assets[799]["sequence"])
        for after in cursors:
            with self.subTest(name="page_deep", after=after):
                self.compare("page_deep", (after,))
            for rating in (0, 1, 2, 3, 4, 5):
                with self.subTest(name="rating", after=after, rating=rating):
                    self.compare("rating", (rating, after))
        for rating in (0, 1, 3, 5):
            self.assertEqual(len(self.compare("rating", (rating, 0))), 200)

    def test_missing_annotations_are_filtered_before_limit(self):
        rows = self.compare("page_deep", (0,))
        self.assertEqual(len(rows), 200)
        self.assertGreater(rows[0][0], self.assets[224]["sequence"])
        # Limiting the first 200 assets before the join would incorrectly return no rows.
        self.assertTrue(all(asset["sequence"] not in self.annotations for asset in self.assets[:200]))

    def test_underfilled_empty_tail_and_no_annotations(self):
        last = self.assets[-1]["sequence"]
        for after in (self.assets[-5]["sequence"], last, last + 1):
            self.assertLess(len(self.compare("page_deep", (after,))), 200)
            for rating in (0, 1, 3, 5):
                self.assertLess(len(self.compare("rating", (rating, after))), 200)
        self.assertEqual(self.compare("page_deep", (last,)), [])
        self.db.execute("DELETE FROM annotations")
        self.annotations.clear()
        self.assertEqual(self.compare("page_deep", (0,)), [])
        self.assertEqual(self.compare("rating", (0, 0)), [])

    def test_current_annotation_values_and_membership_are_preserved(self):
        changed = list(self.annotations)[:30]
        for sequence in changed:
            self.db.execute("UPDATE annotations SET rating=2 WHERE asset_id=?", (sequence,))
            self.annotations[sequence] = 2
        removed = changed[0]
        self.db.execute("DELETE FROM annotations WHERE asset_id=?", (removed,))
        del self.annotations[removed]
        self.compare("page_deep", (0,))
        rows = self.compare("rating", (2, 0))
        self.assertEqual([row[0] for row in rows], changed[1:])


if __name__ == "__main__":
    unittest.main()
