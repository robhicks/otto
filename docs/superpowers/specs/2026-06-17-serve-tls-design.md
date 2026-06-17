# otto Design — TLS / WSS for serve

**Status:** approved design (spec). Implementation plan to follow in `docs/superpowers/plans/`.
**Date:** 2026-06-17

## Goal

Let `otto serve` (the WebSocket transport, PR #24) serve over TLS — `wss://` — so the
`Command`/`Event` stream is encrypted in transit. Opt-in via cert/key flags; the existing
plaintext loopback path is unchanged. Second sub-project of the remote axis.

## Context

`cmd_serve` (`crates/engine/src/main.rs`) currently binds a `tokio::net::TcpListener` and runs
`axum::serve(listener, app)` — plaintext `ws://`, loopback, bearer-token-authed. `serve::app`
returns an `axum::Router` and does not change here; TLS is purely a serving-layer concern at
the bind point. `axum::serve` has no native TLS.

## Decisions (locked during brainstorming)

1. **Opt-in TLS, non-breaking.** New flags `--tls-cert <pem> --tls-key <pem>`: both present →
   `wss://`; neither → plaintext `ws://` (today's behavior); exactly one → startup error.
   Keeps the loopback dev flow and existing tests intact. TLS is not made mandatory (premature
   before a real remote; would break loopback dev).
2. **Library = `axum-server` (0.7, `tls-rustls`).** It serves the same axum `Router` and
   handles both plaintext and TLS from a pre-bound `std::net::TcpListener` (so tests can use an
   ephemeral `:0` port). **Unify the serve path** on it via a `serve::run` helper used by both
   `cmd_serve` and the tests; drop the direct `axum::serve` call.
3. **Certs from PEM files at startup** (`RustlsConfig::from_pem_file`). Generating a cert is a
   dev/test concern (`rcgen`), not a runtime feature.
4. **Fail-closed:** a requested-but-unloadable TLS config is a hard startup error; never a
   silent fallback to plaintext. The bearer token stays required with or without TLS.

## Architecture

### `serve::run` — the unified serving entry (`crates/engine/src/serve.rs`)

```rust
/// Serve `app` on a pre-bound listener, with TLS when `tls` is `Some`. Unifies the plaintext
/// and TLS paths on `axum-server` so both are driven from a std listener (testable on :0).
pub async fn run(
    listener: std::net::TcpListener,
    app: AxumRouter,
    tls: Option<axum_server::tls_rustls::RustlsConfig>,
) -> anyhow::Result<()> {
    match tls {
        Some(cfg) => {
            axum_server::from_tcp_rustls(listener, cfg)
                .serve(app.into_make_service())
                .await?
        }
        None => {
            axum_server::from_tcp(listener)
                .serve(app.into_make_service())
                .await?
        }
    }
    Ok(())
}
```

`app()` is unchanged. The listener is created by the caller (`cmd_serve` binds the configured
addr; tests bind `127.0.0.1:0`), so port selection and TLS are orthogonal to the router.

### CLI wiring (`crates/engine/src/main.rs`, `cmd_serve`)

- Parse `--tls-cert <path>` and `--tls-key <path>` alongside the existing `--port`.
- Resolve TLS:
  - both paths present → `RustlsConfig::from_pem_file(cert, key).await?` → `Some(cfg)` (a load
    error propagates and aborts startup);
  - neither present → `None`;
  - exactly one present → print an error and exit non-zero.
- Bind `std::net::TcpListener::bind("127.0.0.1:<port>")?` (set non-blocking), print the banner
  with the right scheme (`wss://…` when TLS is on, else `ws://…`), and call `serve::run`.
- The bearer-token check (`OTTO_TOKEN` required) is unchanged and independent of TLS.

### Cert provisioning

Runtime: operator supplies PEM cert + key files. Tests: `rcgen` generates a self-signed cert
with a `127.0.0.1` subject-alt-name, written to temp PEM files for the server and trusted by
the test client's root store (real validation, not skip-verify).

## Error handling & determinism

- TLS requested but cert/key missing/unreadable/malformed → hard error at startup, non-zero
  exit. No silent plaintext fallback.
- Exactly one of `--tls-cert`/`--tls-key` → usage error, non-zero exit.
- The default offline test path is unaffected (TLS is opt-in; the plaintext tests still run).
  The TLS test is pure loopback (no external network, no real CA), so the suite stays
  deterministic and key-free.

## Testing

- **WSS round-trip:** `rcgen` self-signed cert for `127.0.0.1`; start the server with TLS on an
  ephemeral port via `serve::run(listener, app, Some(cfg))`; connect with a `tokio-tungstenite`
  rustls `Connector` whose `ClientConfig` **trusts the generated cert** (a real handshake +
  validation); send `SendPrompt`; assert the `Ready` frame and a streamed `TurnComplete`.
- **Plaintext path still works:** the existing serve integration test is adapted to drive the
  server through `serve::run(listener, app, None)` (a std listener instead of `axum::serve`),
  proving the unified path keeps `ws://` working; the auth-rejection tests are retained.
- **One-flag error:** a unit/CLI-level check that supplying only `--tls-cert` (or only
  `--tls-key`) is rejected — exercised at whatever level the arg parsing is testable; if the
  parsing lives only in `cmd_serve`, document the behavior and cover the resolve logic by
  extracting a small `resolve_tls(cert: Option, key: Option) -> Result<Option<...>>`-style
  helper that is unit-testable without binding a socket.

**Implementation latitude (one place):** the `rustls` versions used by `axum-server`,
`tokio-tungstenite`, and any direct `rustls` dev-dep must be compatible (the `Connector::Rustls`
client config type must match tokio-tungstenite's `rustls`). The implementer pins compatible
versions. If the tungstenite-rustls client wiring proves intractable, fall back to asserting a
TLS handshake against the port with a raw `tokio-rustls` client (still proves WSS is live);
keep the assertions' intent fixed.

## Out of scope (named, not silently dropped)

- **mTLS / client certificates** — server-auth TLS only.
- **ACME / Let's Encrypt automation** — operator supplies PEM files.
- **Hot cert reload** — cert changes require a restart.
- **Non-loopback binding / 0.0.0.0** — still binds `127.0.0.1`; exposing the engine beyond
  loopback is a deployment concern for the `RemoteTarget` sub-project.
