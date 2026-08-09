# weavatrix-rust-refactor

Evidence-backed refactor operations for Weavatrix: plan production, transactional
application, and rollback.

This crate is to [`weavatrix-rust`](https://github.com/sergii-ziborov/weavatrix-rust)
what a writer is to a reader. It consumes the read-only evidence graph, produces
`weavatrix.edit-plan.v1` envelopes, and applies them through
[`weavatrix-worktree`](https://github.com/sergii-ziborov/weavatrix-worktree)'s
crash-recoverable transaction. It owns no protocol: the MCP host
[`weavatrix-refactor`](https://github.com/sergii-ziborov/weavatrix-refactor)
composes this catalog with the read-only one.

```text
weavatrix-rust            reusable read-only evidence engine
        |
        +-- weavatrix             read-only MCP host
        |
        +-- weavatrix-rust-refactor   refactor operations  (this crate)
                    |
                    +-- weavatrix-refactor   refactor MCP host
```

Built on the existing refactor kernel rather than a reimplementation of it:
`weavatrix-worktree` for the transaction and its journal, `weavatrix-refactor-plan`
for evidence metadata and canonical fingerprints, `weavatrix-edit` for
Unicode-safe edit-plan validation and application.

## The contract is frozen, not re-derived

The eleven tool names, their JSON schemas and all 47 result states were recorded
from the shipping JavaScript implementation into
[`contract/refactor-tools.v1.json`](contract/refactor-tools.v1.json). That file is
compiled into the crate and is the **only** source of the tool catalog, so this
implementation cannot drift from the schemas agents already depend on.

An operation is conformant when it answers with a status from that file — never a
new one, never a renamed one. Tests enforce both directions: every contract tool
has an operation arm, and no operation exists outside the contract.

Changing a name, a schema or a status means changing the contract file, and that
is a contract-version decision rather than an implementation detail.

## Migration status

The catalog is complete and live; the engines land one at a time. An operation
that has not been ported answers `NOT_SUPPORTED` — itself a contract status — with
a reason naming `weavatrix-refactor-js` as the implementation to use meanwhile.
Nothing is hidden behind a flag, so a client can always tell which half is native.

| Area | State |
| --- | --- |
| Frozen contract, catalog, dispatch | done |
| Safety kernel (containment, UTF-16 ranges, fingerprints, tokens, locking, atomic write, rollback) | provided by `weavatrix-edit` / `weavatrix-worktree` |
| Graph-native planners (rename, SQL rename, bulk replace, symbol edit, move review, delete readiness) | pending |
| JavaScript/TypeScript signature, imports and exact rename | pending, last by design |

## Safety boundary

Producing a plan is a read. Applying one requires all three gates: the host
exposes the edit capability, `WEAVATRIX_ALLOW_SOURCE_EDITS=1` is set, and the call
presents a single-use token bound to that exact plan and repository. Nothing in
this crate writes without them.

## License

MIT.
