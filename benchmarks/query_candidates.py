"""Experimental SQLite query shapes; not installed into any campaign or runtime.

Parameter order and the six-column projection match the frozen queries. These
candidates require correctness and execution-work validation before use; their
SQL shape alone establishes no latency or catalog-size guarantee.
"""

CANDIDATE_SQL = {
    # Keep assets on the left of CROSS JOIN, with the original cursor and order.
    "page_deep": """SELECT a.sequence,a.id,a.folder_id,a.captured_at,r.rating,a.preview_hash
        FROM assets a CROSS JOIN annotations r
        WHERE r.asset_id=a.sequence AND a.sequence>?
        ORDER BY a.sequence LIMIT 200""",
    # The join equates both keys; express the cursor/order on the rating relation.
    "rating": """SELECT a.sequence,a.id,a.folder_id,a.captured_at,r.rating,a.preview_hash
        FROM annotations r CROSS JOIN assets a
        WHERE a.sequence=r.asset_id AND r.rating=? AND r.asset_id>?
        ORDER BY r.asset_id LIMIT 200""",
}
