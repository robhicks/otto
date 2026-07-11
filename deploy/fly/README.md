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

## Limitations
The bundled image includes `git` but not `gh` (GitHub CLI), so `git.pr_open` (opening GitHub
PRs) is unavailable on Fly-provisioned sessions; all other git operations (status/diff/log/add/
commit/branch/checkout/push/etc.) work fine. If PR-opening is needed, extend the image with
`gh` and supply a `GH_TOKEN`.

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
