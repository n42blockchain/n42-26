# Devlog 114 — chain-bound H2-v4 signing domains

Date: 2026-07-21

The first shared H2-v4 object is now fixed in Go and Rust. The five signature
preimages use this common base:

`N42H2V4 || phase || chain_id_le || genesis_hash || view_le`

Proposal and Commit append `block_hash || validator_changes_hash`; Vote appends
`block_hash`; Timeout and NewView use the base only with distinct phase bytes.
This fixes both the old 46/78-byte Commit disagreement and legacy cross-chain
replay of timeout/new-view signatures.

The byte-identical fixture is
`crates/n42-network/testdata/h2_v4_domains_v1.json`, SHA-256
`f3f20d4641455eaf7ea6c96641fc4674134080aefcb300c219ab34a53d4d9510`.
The old engines remain unchanged until the versioned H2-v4 envelope/topic is
ready, so existing 7-node networks do not silently split.

