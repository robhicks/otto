# otto Plan 4d Design — Real ContextFinder (+ Coder reads contents)

**Status:** approved design (spec). Implementation plan to follow in `docs/superpowers/plans/`.
**Date:** 2026-06-15

## Goal

Replace `StubContextFinder` — the last remaining stub in the spine — with a real
ContextFinder that selects the files relevant to a goal, and make the `Coder` actually **read**
those files so their contents inform the edits it produces. Today the Coder only embeds file
*names* in its prompt, so a smarter ContextFinder alone would deliver marginal value; Plan 4d
therefore covers both agents as one coherent, end-to-end unit.

## Decisions (locked during brainstorming)

1. **Retrieval = hybrid:** a deterministic lexical prefilter narrows the workspace to a
   candidate set, then an LLM ranks/selects the most relevant subset — with a deterministic
   lexical fallback so the default offline path stays reproducible.
2. **Scope = ContextFinder + Coder-reads-contents.** Both agents change in this plan.
3. **Recursive listing** is delivered by implementing the long-deferred glob support in
   `LocalWorkspace::list` (whose own comment already earmarks this work), surfaced through the
   existing `fs.list` tool — no tool wire-change.

## Architecture

### Data flow (the trait seam is unchanged)

```
FindContext { goal }
   └─ ContextFinder: lexical prefilter ──► candidate set ──► LLM rank/select (fallback: lexical)
        └─► Context { files: Vec<PathBuf> }            (ranked, capped at 8)
              └─ orchestrator passes files ──► Code { goal, context, feedback, prior_failures }
                    └─ Coder: reads each context file via fs.read ──► prompt with path+contents
                          └─► Code { edits }
```

`AgentOutput::Context` and `AgentRequest::Code` keep `Vec<PathBuf>`. All new capability lives
inside `ContextFinder`, `Coder`, and `LocalWorkspace::list`. The orchestrator is untouched.

### Component A — recursive `fs.list` (`crates/workspace/src/lib.rs`)

`LocalWorkspace::list` currently ignores its `glob` argument and lists the root shallowly
(its comment defers globbing to "the retrieval/ContextFinder work in a later plan" — this plan).
Implement:

- A `glob` containing `**` triggers a **recursive** walk of the workspace; `*` (and any
  non-`**` value) preserves today's **shallow** behavior.
- The recursive walk **ignores** these path components: `.git`, `target`, `node_modules`, and
  any component beginning with `.` (covers `.env`/`.ssh`/`.aws`, consistent with the permission
  gate's sensitive-path floor).
- It does **not follow symlinks** and caps enumeration at **5000 entries** to bound cost.
- It returns relative paths, sorted (deterministic).

Arbitrary glob-pattern matching (e.g. `src/**/*.rs`) is **out of scope** — recursive-vs-shallow
is all the ContextFinder needs, and it avoids adding a glob dependency. The `fs.list` tool
already forwards `glob`, so no change to the tool or its wire shape.

### Component B — real `ContextFinder` (`crates/agents/src/context_finder.rs`, new)

A hybrid agent in two stages.

**Stage 1 — lexical prefilter (deterministic, always runs):**

1. Recursively enumerate files via `fs.list` with a `**` glob.
2. Derive **keywords** from the goal: split on non-alphanumeric, lowercase, keep tokens of
   length ≥ 3, drop a small stopword set (e.g. `the, and, for, with, that, this, add, fix,
   make, use`).
3. **Score** each file as the sum over keywords of `5·(path/filename hits) + 1·(content hits)`
   — filename relevance weighted higher than body matches. Content is read for scoring up to
   **64 KB per file**; non-UTF8/binary files contribute only path hits.
4. Rank by score descending, tie-broken by path ascending (reproducible). Keep the **top 20**
   as the candidate set. Files scoring zero are excluded.

**Stage 2 — LLM rank/select (graceful, may no-op):**

1. Prompt the router with the goal and the candidate paths (with per-file match counts), asking
   for the most relevant subset as JSON `{ "files": ["<relative path>", ...] }`, capped at
   **8** files, ordered most-relevant-first.
2. Parse via the existing `extract_json`. **Keep only paths present in the candidate set**
   (reject hallucinated paths). Cap at 8.
3. **Fallback:** if no JSON / invalid JSON / empty selection (the `LocalProvider` default, or a
   model that doesn't answer in schema), return the **lexical top 8**. This is what keeps the
   default offline path fully deterministic, mirroring the existing Planner/Coder fallbacks.

Output: `AgentOutput::Context { files }`.

### Component C — `Coder` reads contents (`crates/agents/src/coder.rs`)

Before building the prompt, the Coder reads each context file via the gated `fs.read` tool and
embeds labeled `path + contents` blocks (replacing the current names-only list). Budgeting:

- At most **8 files**, at most **8 KB per file** (truncate the remainder with an explicit
  `… (truncated)` marker), at most **32 KB total** across all injected files.
- Files that are unreadable, gate-denied, or non-UTF8 are **skipped gracefully** (the Coder
  still runs; it just sees fewer files).

The existing no-JSON edit fallback and the `feedback`/`prior_failures` repair threading are
unchanged.

### Component D — wiring (`crates/engine/src/lib.rs`)

`build_default_registry` registers the real `ContextFinder` for `Role::ContextFinder`;
`StubContextFinder` and its impl are removed from `crates/agents/src/lib.rs`. The crate-level
doc comment is updated to note the whole spine is now real (no remaining stubs).

## Determinism & error handling

- **Default offline (`LocalProvider`):** Stage 2 falls back to lexical ranking and the Coder
  reads files deterministically → the full turn is reproducible with no network or API keys.
  The determinism invariant is preserved.
- **Empty workspace / no keyword matches:** candidate set empty → `Context { files: [] }` →
  Coder behaves as it does today (no injected files).
- **Binary / non-UTF8 files:** skipped for content scoring and for content injection.
- **Sensitive files:** excluded by the walk's ignore-list and denied by `fs.read`'s gate
  regardless — defense in depth.
- **Tool errors** (e.g. `fs.list`/`fs.read` failures): degrade to the best available result
  (empty listing → empty context; unreadable file → skipped) rather than failing the turn.

## Testing

- **Workspace `list`** (`crates/workspace`): seed nested directories; assert recursive
  enumeration with `**`, shallow behavior preserved with `*`, ignore-list exclusions
  (`target/`, `.git/`, dotfiles), symlinks not followed, and the entry cap.
- **ContextFinder lexical** (`crates/agents`): deterministic ranking over a seeded tempdir
  (goal keywords → expected file order), candidate cap, tie-break by path, zero-score exclusion.
- **ContextFinder LLM stage:** `ScriptedProvider` returns a JSON subset → asserted selection;
  invalid JSON → lexical fallback; a path not in the candidate set → filtered out.
- **Coder reads contents:** `ScriptedProvider` asserts injected file contents appear in the
  prompt; per-file and total truncation; a gate-denied/unreadable file is skipped.
- **Integration** (`crates/engine`): a full turn over a tempdir exercising ContextFinder → Coder
  end-to-end, asserting the produced edit reflects injected context.

All new tests are offline and deterministic (lexical paths use a tempdir workspace; LLM paths
use `ScriptedProvider`). Per-crate then workspace gates; `clippy -D warnings` clean; `fmt`
clean; TDD throughout.

## Scope boundaries (YAGNI / deferred)

- No arbitrary glob-pattern matching (recursive vs. shallow only).
- No embedding/semantic retrieval — lexical + LLM ranking only.
- No `.gitignore` parsing (fixed ignore-list).
- No change to the `Context`/`Code` wire payloads (`Vec<PathBuf>`).
- Other project types for the Verifier, Planner-milestones-into-Coder, and a read-only
  workspace view for untrusted agents remain separate future work.

## File structure

```
crates/workspace/src/lib.rs           # MODIFY: recursive glob support in `list` + ignore-list
crates/agents/src/context_finder.rs   # NEW: hybrid ContextFinder (lexical → LLM, fallback)
crates/agents/src/coder.rs            # MODIFY: read context files, inject path+contents (budgeted)
crates/agents/src/lib.rs              # MODIFY: re-export ContextFinder; remove StubContextFinder
crates/engine/src/lib.rs             # MODIFY: register the real ContextFinder
docs/ARCHITECTURE.md                  # MODIFY: document the real ContextFinder + recursive list
```

## What Plan 4d delivers

The entire spine is real: **Planner → ContextFinder → Coder → Verifier**, with no stubs left.
The ContextFinder finds the files that matter (lexically, refined by the model when one is
available) and the Coder reads them, so edits are grounded in actual file contents — closing the
loop from goal to context-aware change, while the default offline path stays fully deterministic.
