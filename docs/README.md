# N42 Documentation Set

This directory is a structured, release-oriented documentation layer for the `n42-26` workspace.
It is intended to complement the historical engineering notes already present in [`docs/`](docs), not replace them.

## Reading order

1. [00-Workspace-Map.md](Docs/00-Workspace-Map.md)
2. [01-System-Architecture.md](Docs/01-System-Architecture.md)
3. [02-Core-Flows.md](Docs/02-Core-Flows.md)
4. [03-Crate-Reference.md](Docs/03-Crate-Reference.md)
5. [performance-records.md](performance-records.md)
6. Module deep dives under [`Docs/modules/`](Docs/modules)
7. Audit and maturity register under [`Docs/issues/`](Docs/issues)

## Scope

This documentation covers:

- workspace topology and crate ownership
- startup path and runtime composition
- consensus, execution, networking, mobile verification, rewards, JMT, and ZK flows
- binaries, test harnesses, and operational surfaces
- performance records and benchmark caveats
- known issues and audit findings worth tracking before release

## Source basis

The content here was produced from the current workspace layout, public module exports, entrypoints, selected orchestration files, and existing project documentation such as [`README.md`](README.md) and the lower-case [`docs/`](docs) devlogs.

## Important conventions

- `docs/` remains the historical devlog archive.
- `Docs/` is the structured reference set for architecture, operations, and audit status.
- `performance-records.md` is the stable performance index; it separates production-like TPS from benchmark-only upper-bound runs such as the batch transfer fast lane in `devlog-83`.
- Flow diagrams use Mermaid so they can be rendered in supporting viewers.
- “Known issues” include both open items and recent security findings that should be tracked explicitly.
