# Model routing (extensions slice 8) — design

## Problem

Extensions already parse a `model:` field on custom commands (`CommandDef.model`) and custom
agents (`CustomAgentDef.model`), but it is **inert**: `build_router()` ignores it on every CLI
path, so declaring `model: claude-opus-4-8` in a command or agent changes nothing.

The tension is that otto's routing axis is **local-vs-remote slots** — each slot has a model
baked in at construction (`AnthropicProvider`/`OllamaProvider`, sourced from `OTTO_*_MODEL`) —
while Claude Code's `model:` names a *specific* model. There is no "use this exact model"
concept anywhere in the request path (`CompleteRequest { prompt }` carries no model; the
`Router` seam only chooses a slot from `RouteHints`).

This slice makes the parsed `model:` field take effect.

## Semantics

A command/agent's `model:` field **pins the remote slot's model for that whole turn**:

- **Honorable** (`ANTHROPIC_API_KEY` present): the turn's remote provider is an
  `AnthropicProvider` built with the named model id (not `OTTO_ANTHROPIC_MODEL`), and routing
  prefers remote so the named model actually runs.
- **Privacy floor stays inviolable**: a privacy-sensitive request still forces local, dropping
  the model override. This preserves the existing "privacy always forces local" invariant.
- **Not honorable** (no key, or only `LocalProvider` configured): fall back to the normal
  env-based router and print a loud stderr warning. The offline/deterministic default is
  untouched.

The model string passes **straight through** to the Anthropic provider as the model id, exactly
as `OTTO_ANTHROPIC_MODEL` does today. No alias→id translation this slice.

## New router: `PinnedModelRouter`

Lives in `crates/router/src/lib.rs`, mirroring `BrainBlendRouter`:

```rust
pub struct PinnedModelRouter {
    local: Arc<dyn Provider>,
    remote: Arc<dyn Provider>, // AnthropicProvider built with the pinned model id
}
```

`complete(req, hints)`:

- `hints.privacy_sensitive` → route **local** (never send a privacy-sensitive request to the
  pinned remote model).
- otherwise → route **remote**. On a remote error, fall back to local for liveness — the same
  cross-provider fallback `BrainBlendRouter` performs for non-privacy requests.
- a **privacy** request that errors on local surfaces the error; it is never re-sent remote
  (no crossing the privacy boundary), matching `BrainBlendRouter`.

It deliberately ignores complexity and `prior_failures` escalation: the user named a model, so
the router honors it rather than second-guessing the tier.

## Wiring in `crates/engine/src/lib.rs`

- Extract `build_local_provider() -> Arc<dyn Provider>` from today's local-slot logic (the
  `OTTO_OLLAMA` branch), so both router builders share it.
- Add `build_router_with_model(model_override: Option<&str>) -> Box<dyn Router>`:

  | `ANTHROPIC_API_KEY` | `model_override` | Result |
  |---|---|---|
  | present | `Some(m)` | `AnthropicProvider(m)` + `PinnedModelRouter` |
  | present | `None` | today's `BrainBlendRouter` (unchanged) |
  | absent | `Some(m)` | warn to stderr, `SingleProviderRouter(local)` |
  | absent | `None` | `SingleProviderRouter(local)` (unchanged) |

- `build_router()` becomes `build_router_with_model(None)` — every existing caller is
  byte-for-byte unchanged.

## CLI plumbing in `crates/engine/src/main.rs`

- **`run_command_in`**: read `def.model.as_deref()` and pass it to `build_router_with_model(...)`
  (replacing the bare `build_router()`). `def` is owned and still alive at that point (only
  `&def.template` and `&def.allowed_tools` are borrowed earlier), so `def.model` is readable.
- **`run_custom_agent_in`**: the agent runs via `TaskTool`, which receives a single `router`.
  Capture the target agent's `def.model` before the defs move into the registry (during the
  existing `for def in ext.agents` loop), then build the router with it.

  **Limitation (documented, not fixed):** only the top-level `--agent` model pins the router;
  nested `TaskTool` sub-dispatches inherit that same pinned router. Per-sub-agent model would
  require threading `model` through the `Agent` seam (the deferred "thread through the seam"
  approach) and is out of scope.

## Determinism

With no `ANTHROPIC_API_KEY` (the test/CI default), a `model:` field never changes routing —
always `SingleProviderRouter(local)`; only a stderr warning appears. The offline determinism
suite stays reproducible.

## Testing

- **`PinnedModelRouter`** unit tests (fake providers, following the existing router test
  pattern): remote for non-privacy; local for privacy; non-privacy remote-error → local
  fallback; privacy remote-error → surfaced with no cross-boundary fallback.
- **Engine**: `build_router_with_model(Some(m))` with no key stays offline-deterministic
  (assert a deterministic completion). Real Anthropic pinning is not network-testable; it is
  covered structurally by the `PinnedModelRouter` unit tests plus the obvious wiring.
- **`main.rs`**: the existing `run_command_in` / custom-agent tests gain a `model:` case
  asserting the turn still runs deterministically offline (the graceful-fallback path).

## Out of scope (deferred)

- The `serve` path (its own router/session lifecycle) and the plain `otto run` spine (no model
  source).
- Skills' `model` (inert by design — a skill has no invocation scope to pin).
- Per-sub-agent model / threading `model` through the `Agent` seam.
- Alias→model-id translation.

## Files touched

1. `crates/router/src/lib.rs` — `PinnedModelRouter` + unit tests.
2. `crates/engine/src/lib.rs` — `build_local_provider()`, `build_router_with_model()`,
   `build_router()` wrapper.
3. `crates/engine/src/main.rs` — `run_command_in` and `run_custom_agent_in` pass the def's
   model; add `model:` test cases.
4. `CLAUDE.md` + this spec / the implementation plan — record the slice.
