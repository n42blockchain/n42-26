# sync v1.1 golden vectors (gov5 reference)

Copied unchanged from gov5's `internal/testdata/sync-v11` (branch `feat/p1-sync-v11`,
`TestSyncV11Vectors`), as published with N42-SYNC-SPEC.md. Used by
`crates/n42-network/src/sync_v11/tests.rs` (`bodies_by_range/2`, §5.1; `commit_qc/1`,
§5.2) and by the commit-QC verification test in `n42-consensus-service`.

    27d5edfc766de64d87c1dc0a5d16ce34624ca91407fb3f2595a6f54ad60217d6  bodies_by_range_2.json
    bf1ab9a872868730a809b13b2a19c6d8c5a56d063dc2d4a094a759b8108d1fe9  commit_qc_1.json

Snappy framing in `request_wire` / `response_wire` is golang/snappy's writer; tests
compare payloads, result codes, context bytes and END trailers, not whole wires.
