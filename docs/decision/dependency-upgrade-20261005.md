# DDN dependency refresh — 2026-10-05

## Versions

The official Reth releases page lists v2.7.0 as the latest stable release. The
workspace already contains its N42 compatibility port, pinned to upstream
`3d592ece6de8c4559987416a544fc215fd6d6921` and adapted revision
`3452d7d0217095446b0f2632a283e314951da273`; this refresh retains that source pin.

| Dependency | Previous manifest | Updated manifest |
| --- | --- | --- |
| DDN relay reqwest | 0.12.28 | 0.13.5 |
| DApp viem | 2.37.8 | 2.57.3 |
| reth-primitives-traits | 0.8.1 | 0.8.2 |
| alloy-primitives / alloy-sol-types | 1.6.1 | 1.7.3 |
| tokio | 1.53.1 | 1.53.2 |
| thiserror | 2.0.19 | 2.0.21 |
| rcgen | 0.14.8 | 0.14.10 |

The reqwest 0.13 port changes its TLS feature from `rustls-tls` to `rustls`,
which selects the upstream AWS-LC provider and platform certificate verifier.
The relay's existing JSON request and response APIs compile without further
changes. The DApp keeps an explicit version in its browser ESM import.

`cargo update --offline` refreshed compatible cached packages, including
auto_impl, mio, powerfmt, want, and zerocopy. Some new manifest minimums were
already resolved in the previous lockfile. Offline resolution does not establish
that every transitive dependency is the newest version published online.

The Jev model and registered template hashes remain versioned protocol inputs;
changing the model requires registering and deploying a new template. The
bincode wire format also retains its existing protocol pin.

## Validation and source location

The October 6 DDN commits isolate the contracts, SDK, relay, DApp, and their
dependency refresh from the pre-existing Reth migration and performance work.
Those Reth migration changes remain in the local working tree; the committed
workspace retains its earlier Reth source baseline. The committed lockfile was
resolved separately from that baseline, updating 15 package entries for the
DDN refresh. The relay passed `cargo check --all-targets --locked` using that
exact committed dependency snapshot. The checks below describe the original
October 5 working-tree validation, which used the isolated Reth v2.7.0 source.

The original `../reth` checkout is older than `reth-source.lock`. Validation uses
the existing isolated `.artifacts/reth-upgrade/reth-worktree` source. A copied
workspace under `.artifacts/ddn-dependency-upgrade/workspace` redirects Reth paths
to that source, leaving the original checkout intact. Normal builds still require
preparing `../reth` as documented in the repository README.

- All six Reth integration patches passed `scripts/check-reth-source.sh`.
- `cargo check -p n42-decision-relay --all-targets --offline --locked` passed
  against the copied workspace and adapted Reth source.
- `cargo test -p n42-decision-relay --offline --locked` passed all 15 existing
  tests against the same source.
- The SDK's existing Node test passed, and the DApp passed `node --check`.
- `cargo check -p n42-network --lib --offline --locked` could not proceed because
  rcgen 0.14.10 requires `pem 4.0.0`, which is absent from the local source cache.
  This environment cannot resolve external hosts, so downloading it is blocked.
- Browser loading of the updated viem CDN import and live Jev/RPC integration
  have not been verified.

After network access is available and `../reth` is prepared, run
`cargo check --workspace --all-targets --locked` to validate the shared dependency
refresh. No running network deployment is part of this source update.

## Upstream references

- [Reth releases](https://github.com/paradigmxyz/reth/releases)
- [reqwest 0.13.5](https://github.com/seanmonstar/reqwest/releases/tag/v0.13.5)
- [reqwest TLS features](https://docs.rs/reqwest/0.13.5/reqwest/)
- [viem 2.57.3](https://github.com/wevm/viem/releases/tag/viem%402.57.3)
