# Deploying otto on Fly.io

`FlyTarget` (mode `--promote-fly`) provisions one Fly app + machine per session from an
`otto-serve` image. Steps:

## 1. Build & push the image
```bash
fly auth docker
docker build -t registry.fly.io/otto-serve:latest -f deploy/fly/Dockerfile .
docker push registry.fly.io/otto-serve:latest
```

## 2. Configure the source engine
```bash
export FLY_API_TOKEN=$(fly auth token)      # provisioning credential
export OTTO_FLY_ORG=personal
export OTTO_FLY_REGION=iad
export OTTO_FLY_IMAGE=registry.fly.io/otto-serve:latest
export OTTO_TOKEN=<source bearer>           # required by `otto serve`
```

## 3. Serve with Fly provisioning
```bash
otto serve --promote-fly
```
Promoting a session then creates an app with a unique random suffix (e.g.
`otto-session-<random>.fly.dev` — the suffix is a random 12-hex string, not the session id),
runs `otto serve` on it, and the client reconnects. Demote/stop destroys the app immediately.
Idle machines suspend (`autostop=suspend`), which halts compute billing; an orphaned machine
(e.g. the source engine crashes and the client never demotes) stays suspended — reclaimed by
the manual `fly apps destroy` sweep below, since `auto_destroy` only fires once a machine fully
stops.

## Guest tools & limitations
The image ships the engine plus the `mcp-fs`, `mcp-grep`, and `mcp-git` servers, so a Fly-provisioned
session has file (`fs.*`), search (`grep`), and git (`git.*`) tools. Not available on the guest:

- `git.pr_open` — the image includes `git` but not `gh` (GitHub CLI); all other git operations
  (status/diff/log/add/commit/branch/checkout/push/etc.) work fine. Extend the image with `gh` and a
  `GH_TOKEN` if you need PR-opening.
- `bash` / sandboxed tools — no OS sandbox backend (bwrap) is installed, so `bash` stays fail-closed
  (unregistered), consistent with otto's "no backend → no bash" rule.
- `lsp.*` — no language servers are on PATH, so the LSP tools don't register.

The image sets `OTTO_HOST=0.0.0.0` so Fly's proxy (which reaches the machine over its network
interface, not loopback) can connect — no operator action needed. Run standalone, `otto serve`
otherwise defaults to binding `127.0.0.1` for safe local use.

## Env reference
`FLY_API_TOKEN`, `OTTO_FLY_ORG`, `OTTO_FLY_REGION`, `OTTO_FLY_IMAGE`, `OTTO_FLY_CPUS` (1),
`OTTO_FLY_MEM_MIB` (1024), `OTTO_FLY_APP_PREFIX` (otto-session), `OTTO_FLY_PORT` (8787),
`OTTO_FLY_BOOT_TIMEOUT_MS` (30000).

## Cleanup of orphan empty apps (follow-up)
A suspended orphan machine's (free) app remains until swept — `auto_destroy` doesn't apply
since the machine never reaches the stopped state. Sweep periodically:
```bash
fly apps list | grep otto-session- | awk '{print $1}' | xargs -n1 fly apps destroy -y
```
