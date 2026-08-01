# otto

An agentic coding engine. A deterministic orchestrator drives a spine of small, atomic
agents — **Planner → ContextFinder → Coder → Verifier** — over a workspace, routing LLM
calls across local and remote providers behind a stable set of trait seams. The frontend
never branches on "local vs remote": it speaks one protocol to an engine that may
be embedded in-process or served over the network.

> **Status: the single-machine spine is real, and the distribution axis works end to end on
> loopback.** The orchestrator, all four agents (Planner, ContextFinder, Coder, Verifier),
> trait seams, providers, router, tools, sandbox, and permission gate are implemented and
> tested — no stubs remain. The agents are LLM-backed with a deterministic offline fallback, so
> the test suite runs without keys or network. Also landed: **persistence** (sqlite session +
> event-log store), **`otto serve`** (bearer-authed WebSocket with `Last-Event-ID` reconnect,
> plaintext or `wss://`), **`RemoteWorkspace`** + `promote()`/`LoopbackTarget`, the **MCP tool
> servers** (`mcp-fs`/`mcp-grep`/`mcp-git`/`mcp-bash`/`mcp-lsp`), and the **UI** — a single
> Dioxus CSR crate ([`ui-dioxus/`](ui-dioxus/)) compiled to WASM for the browser and natively
> via Dioxus Desktop, replacing the original Leptos + Tauri stack. On-demand
> Fly.io provisioning has since shipped too (`otto serve --promote-fly`; see [`deploy/fly/`](deploy/fly/)).
> The full intended design lives in
> [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md); per-plan history is in `docs/superpowers/plans/`.

## Install

Prebuilt binaries for Linux (`x86_64`, `aarch64`) and macOS (`x86_64`, `arm64`) are attached to
each [GitHub release](https://github.com/robhicks/otto/releases). Linux ships both a glibc build
(`...-unknown-linux-gnu`) and a static musl build (`...-unknown-linux-musl`, for Alpine and older
glibc distros). Release archives are named `otto-<target>.tar.gz` with a per-archive `.sha256`
checksum (a sidecar named `otto-<target>.sha256`), and Linux releases also ship native
`.rpm` packages (Fedora / RHEL-family). Any of these work:

**curl installer** — detects the platform, verifies the checksum, installs to `~/.local/bin`:

```bash
curl -fsSL https://raw.githubusercontent.com/robhicks/otto/main/deploy/install.sh | sh
```

**Homebrew** — the formula in [`deploy/homebrew/otto.rb`](deploy/homebrew/otto.rb) tracks the
current release:

```bash
brew install --formula https://raw.githubusercontent.com/robhicks/otto/main/deploy/homebrew/otto.rb
```

**Fedora / RHEL-family (dnf)** — the native `.rpm` recommends `bubblewrap`, so dnf installs the
sandbox backend automatically. Swap `x86_64` for `aarch64` on an ARM machine:

```bash
sudo dnf install -y https://github.com/robhicks/otto/releases/download/v0.3.0/otto-0.3.0-1.x86_64.rpm
```

(Or download the `.rpm` asset from the release and `sudo dnf install -y ./otto-*.rpm`.)

**From source** — pinned stable Rust toolchain (`rust-toolchain.toml`; edition 2024, rust 1.85):

```bash
git clone https://github.com/robhicks/otto.git && cd otto
cargo build --release -p otto-engine
./target/release/otto --version
```

Runtime requirements: the sandbox behind `otto run` needs `bwrap` on Linux (`sandbox-exec` on
macOS); `otto serve` requires `OTTO_TOKEN`. See "How it works" below.

## Quick start

If you installed a prebuilt binary, `otto` is on your PATH; from a source checkout use
`cargo run -p otto-engine -- <args>`:

```bash
cargo build --workspace
cargo test --workspace          # offline & deterministic — no API keys or network needed
```

Run a single turn against a directory:

```bash
cargo run -p otto-engine -- run "add a hello function" --root /path/to/repo
```

This prints the sequenced event stream (`AgentStarted`, `FileEdit`, `VerifyResult`,
`TurnComplete`, …) and exits non-zero if the turn fails.

With no environment configured, the engine runs **fully offline and deterministically** —
both router slots use the in-process `LocalProvider`. To use real models:

| Variable | Effect |
|---|---|
| `OTTO_OLLAMA=1` | Local slot uses Ollama (`OTTO_OLLAMA_MODEL`, default `llama3.2`). |
| `ANTHROPIC_API_KEY=…` | Remote slot uses Anthropic (`OTTO_ANTHROPIC_MODEL`, default `claude-haiku-4-5`); enables the Brain-Blend router. |
| `DEEPSEEK_API_KEY=…` | Remote slot uses DeepSeek (`OTTO_DEEPSEEK_MODEL`, default `deepseek-v4-flash`); enables the Brain-Blend router. |

## How it works

The engine is one Cargo workspace whose dependencies flow strictly **inward**: `protocol`
depends on nothing, `engine-core` defines the trait seams, the impl crates depend on
`engine-core`, and `engine` wires everything together.

```
protocol        wire types (Command/Event/Role/SessionId) — no I/O
engine-core     orchestrator state machine + the trait seams + AgentRegistry
  ├─ agents     Agent impls (Planner / ContextFinder / Coder / Verifier) — all LLM-backed
  ├─ providers  Provider impls: Local, Scripted, Ollama, Anthropic
  ├─ router     SingleProviderRouter, BrainBlendRouter (privacy/complexity routing)
  ├─ tools      fs.* + bash tools, permission gate, OS sandbox
  └─ workspace  LocalWorkspace (edits a real folder in place; agents see a read-only view)
engine          the `otto` binary + wiring (build_router, build_tool_registry, run_goal)
```

**A turn** (`Orchestrator::run_turn`) is deterministic control flow: Plan → Execute (find
context → code → apply gated edits → Verify, looping to Repair on failure) → Done. The
Verifier detects the project's ecosystem (Rust / Go / Node / Python / Make) and runs its
test command in the sandbox. Agents hold no global state; they receive an `AgentCtx`
granting scoped access to the router, a **read-only** workspace view, and the tool registry.

**Every tool call and every Coder edit passes a permission gate** before it touches disk.
A sensitive-path floor (`.env*`, `.ssh/`, `.git/`, `.aws/`, ssh keys) is always denied, and
edits apply only on an explicit `Allow` (an `Ask` or `Deny` is skipped — fail-closed). The
`bash` tool is registered only when an OS sandbox backend (`bwrap`/`sandbox-exec`) is
present, and runs network-isolated with the workspace root as the only writable path.

The trait seams — `Agent`, `WorkspaceRead`/`Workspace`, `Provider`/`Router`, `Tool` — are
the extension points. They are `Send + Sync` async traits, and the orchestrator only ever
holds trait objects, so new agents, providers, routing policies, or tools slot in without
touching the spine.

## Serving the engine + the browser UI

The same protocol runs embedded (the `run` command above) or served. `otto serve` exposes the
`Command`/`Event` protocol over a bearer-authed WebSocket; the bearer token is mandatory:

```bash
OTTO_TOKEN=devtoken cargo run -p otto-engine -- serve --port 8787   # prints ws://127.0.0.1:8787/ws
```

The UI is a single Dioxus CSR crate in [`ui-dioxus/`](ui-dioxus/). It compiles to WASM for the
browser and natively (via Dioxus Desktop) for a native window — a **standalone crate, intentionally
excluded from the cargo workspace**, depending only on `protocol`.

The UI ships in English, German, Spanish, Hindi, and Simplified Chinese. It follows the browser's
(or, on desktop, the OS's) language by default; the picker in the status strip overrides that and
remembers the choice across restarts.

Build and run it from inside `ui-dioxus/`:

```bash
cd ui-dioxus
cargo test --features desktop               # host-side unit tests
cargo build --target wasm32-unknown-unknown --features web  # wasm compile check
dx serve                                    # dev server in a browser tab (needs `cargo install dioxus-cli`)
```

Desktop auto-launches a bundled `otto serve` sidecar and auto-connects with a folder picker.
For the served web UI, pass the bundle to `otto serve --ui-dir <path>` after building it:

```bash
cd ui-dioxus && ./scripts/build-web.sh      # produces the web release bundle
OTTO_TOKEN=devtoken cargo run -p otto-engine -- serve --port 8787 --ui-dir target/dx/otto-desktop/release/web/public
```

## Developing

```bash
cargo test -p otto-engine-core            # one crate
cargo test -p otto-tools bash::           # filter by test path/name
cargo fmt --all
cargo clippy --workspace --all-targets
```

Tests live alongside the code (`#[cfg(test)] mod tests`). Patterns to follow:

- **Agents** are unit-tested against a `ScriptedProvider` (canned responses keyed by a
  prompt substring) — no network, fully reproducible.
- **The orchestrator** is tested with mocked agents (see the fake-agent structs in
  `crates/engine-core/src/orchestrator.rs`).
- **Provider HTTP behavior** is tested with `wiremock`; **workspace/fs** with `tempfile`.

Keep the offline default deterministic: `LocalProvider`/`ScriptedProvider` perform no I/O,
and anything reading environment variables belongs behind `build_router`, not in core logic.

See [`CLAUDE.md`](CLAUDE.md) for the working conventions and the security rules that are
easy to get wrong, and [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the full design.

## License

MIT OR Apache-2.0.
