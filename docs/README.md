# N42 Documentation Set

This directory is a structured, release-oriented documentation layer for the `n42-26` workspace.
It is intended to complement the historical engineering notes already present in [`docs/`](/Users/jieliu/Documents/n42/n42-26/docs), not replace them.

## Reading order

1. [00-Workspace-Map.md](/Users/jieliu/Documents/n42/n42-26/Docs/00-Workspace-Map.md)
2. [01-System-Architecture.md](/Users/jieliu/Documents/n42/n42-26/Docs/01-System-Architecture.md)
3. [02-Core-Flows.md](/Users/jieliu/Documents/n42/n42-26/Docs/02-Core-Flows.md)
4. [03-Crate-Reference.md](/Users/jieliu/Documents/n42/n42-26/Docs/03-Crate-Reference.md)
5. Module deep dives under [`Docs/modules/`](/Users/jieliu/Documents/n42/n42-26/Docs/modules)
6. Audit and maturity register under [`Docs/issues/`](/Users/jieliu/Documents/n42/n42-26/Docs/issues)

## Scope

This documentation covers:

- workspace topology and crate ownership
- startup path and runtime composition
- consensus, execution, networking, mobile verification, rewards, JMT, and ZK flows
- binaries, test harnesses, and operational surfaces
- known issues and audit findings worth tracking before release

## Source basis

The content here was produced from the current workspace layout, public module exports, entrypoints, selected orchestration files, and existing project documentation such as [`README.md`](/Users/jieliu/Documents/n42/n42-26/README.md) and the lower-case [`docs/`](/Users/jieliu/Documents/n42/n42-26/docs) devlogs.

## Important conventions

- `docs/` remains the historical devlog archive.
- `Docs/` is the structured reference set for architecture, operations, and audit status.
- Flow diagrams use Mermaid so they can be rendered in supporting viewers.
- “Known issues” include both open items and recent security findings that should be tracked explicitly.
