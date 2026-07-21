# Devlog 115 — H2-v4 envelope and observer

Date: 2026-07-21

Go and Rust now share a versioned H2-v4 envelope around the seven-message
legacy payload set. The envelope binds `chain_id`, full `genesis_hash`, and
`validator_changes_hash`, has an exact bounded inner length, and is transported
as raw Snappy on `/n42/h2/4/ssz_snappy`.

The cross-language fixture and gov5-produced Snappy frame are byte-identical
at SHA-256
`09a98f549fcfa1b4185b78b975fa680608c73e169758cb0c052c72efbff4ff83`.

Rust observer mode subscribes to both the legacy fork topic and H2-v4 topic.
It validates the v4 envelope against the local chainspec identity before
emitting a read-only event. Messages are not translated into native consensus
events yet, so this change cannot alter validator voting or the existing
7-node fast path.

