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

## Operations

All eleven are native. Dispatch has no fallback arm, so a tool added to the
contract fails to compile rather than answering "not supported" at run time.

| Operation | Backend | What it proves |
| --- | --- | --- |
| `rename_symbol` | graph + lexical | The declaration, every site the graph proves calls or references it, and the import lines of files the graph proves import the declaring file |
| `rename_related_symbols` | the same, merged | One transaction for several renames, with collision and contested-site detection; chains and swaps apply simultaneously rather than in sequence |
| `change_signature` | graph + token-split lists | A parameter added or removed, with the argument at each proven call site |
| `organize_imports` | occurrence count | Named JS/TS bindings whose identifier occurs once in the file |
| `edit_symbol` | parser range | A symbol-anchored replacement or insertion, parse-gated for JS/TS |
| `bulk_replace` | lexical | Literal or regex matches outside strings and comments |
| `move_file` | specifier arithmetic | The file, plus the relative import specifiers that pointed at it |
| `move_symbol` | graph | A move reviewed against its dependencies before anything is written |
| `delete_readiness` | graph | Whether anything still depends on the symbol |
| `apply_edit_plan` | `weavatrix-worktree` | Preview, single-use token, atomic write with retained contents |
| `rollback_last_apply` | `weavatrix-worktree` | The previous contents, restored |

`rename_symbol` and `rename_related_symbols` own their complete two-phase workflow: preview
returns a plan-bound confirmation token, and repeating the same operation with identical rename
arguments, `mode="apply"`, and that token applies the recomputed plan. The agent never has to
echo the edit plan into `apply_edit_plan`; that generic tool remains available for plans produced
by the other operations or by an external planner.

### What "PARTIAL" means here

No planner claims `COMPLETE`. Every call site comes from a graph edge, so these
operations prove the sites they edit — they cannot prove the *absence* of other
references. A same-named occurrence the graph does not vouch for is reported as an
`UNPROVEN_OCCURRENCE` rather than edited, which is the whole difference between
renaming a symbol and find-replacing a string.

`NOT_SUPPORTED` survives only as a per-call answer where an engine cannot prove
something about the input it was given — a symbol the graph records under a name
that is not an identifier, for instance. It is never the answer for a whole tool.

Outside JavaScript and TypeScript, `organize_imports` reports candidates and
answers `UNPROVEN` instead of planning. `use std::io::Write` is used by calling
`write_all` and never by naming `Write`, and a Python import in `__init__.py` is
often the public API; both pass an occurrence count and both break on removal.

## Safety boundary

Producing a plan is a read. Applying one requires all three gates: the host
exposes the edit capability, `WEAVATRIX_ALLOW_SOURCE_EDITS=1` is set, and the call
presents a single-use token bound to that exact plan and repository. Nothing in
this crate writes without them.

## License

MIT.
