# hypha

**One binary on every server. One panel. No more ssh.**

hypha is a star-topology fleet manager: a hub you run somewhere reachable, and a
tiny agent (`hypha-sat`) on every machine. The agent dials **out** to the hub, so
there are no inbound ports to open, no keys to distribute per host, and no VPN.
Everything you do — run a command on 40 boxes, open a shell, watch the output
stream — goes over that one long-lived connection.

```
   you ──browser──► ┌───────────┐        ┌──────────────────────┐
                    │ mycelium  │ ─────► │        hyphad        │ ◄── your CLI
                    │  (panel)  │  /api  │  api · agent · audit │
                    └───────────┘        └──────────┬───────────┘
                                      outbound ws  │
                                ┌──────────────────┼──────────────────┐
                             ┌──▼───┐           ┌──▼───┐           ┌──▼───┐
                             │ sat  │           │ sat  │           │ sat  │
                             └──────┘           └──────┘           └──────┘
```

## Why not ssh

`ssh host cmd` in a loop is N TCP connections, N auth handshakes, N processes,
zero state, and an inbound port on every machine. hypha gives you:

- **one connection per host**, reused for everything
- **fan-out as a first-class verb** — `hypha run -- 'systemctl restart nginx'`
  hits the whole fleet, streams interleaved output, aggregates exit codes
- **ptys that outlive your laptop** — the shell lives on the agent; the relay is
  what comes and goes
- **an audit log** you can read at 3am
- **zero inbound ports** on workers

## Quickstart

### 1. build

```sh
cargo build --release
# static binaries (recommended for the fleet):
scripts/build.sh
```

### 2. run the hub

```sh
sudo mkdir -p /var/lib/hypha
sudo ./target/release/hyphad --listen 0.0.0.0:5522 --public-url https://hub.example.com
```

**There is no login.** The hub assumes it is reachable only from a private
network (Tailscale, WireGuard, loopback). Anybody who can reach it can run
commands as root on every enrolled host — so don't expose it to the internet. If
you need a public hub, put an auth proxy in front of it.

`hyphad` speaks plain HTTP on purpose and ships **no UI**. Run it on loopback,
put a reverse proxy in front of it for TLS, and serve the panel from
[mycelium](https://github.com/JoeWest9966/mycelium). See
`dist/nginx-hypha.conf` for the full route map, or `mycelium/deploy/proxy.mjs`
if you have no web server.

### 3. add a server

Panel → **＋ Add host** → copy the command → run it on the target as root:

```sh
curl -fsSL https://hub.example.com/install.sh?token=e_xxx | sudo sh
```

It downloads `hypha-sat`, enrolls, installs a systemd unit, starts it. The host
appears in the panel in about a second.

Two kinds of install command:

| | One-shot (default) | Reusable |
|---|---|---|
| lifetime | 24h | 30 days |
| uses | 1 | unlimited, counted |
| listed in panel | no | yes, with **Copy** / **Revoke** |
| for | one new machine | a command you keep in your notes |

Pick **Reusable · 30 days** in the Add host dialog, name it, and it stays in the
panel so you can come back and copy it next week. Revoke it if it leaks. From
the CLI: `hypha add --reusable -n homelab`, `hypha keys`,
`hypha keys --revoke <token>`.

A reusable install command is a key to your fleet. Don't paste it into chat,
screenshots, or dotfiles in a public repo.

### 4. use it

```sh
hypha use https://hub.example.com
hypha hosts
hypha run -- 'uptime'                       # every host
hypha run web01 db01 -- 'systemctl status nginx'
hypha run -t prod -- 'df -h /'
hypha sh web01                              # full tty, detach with Ctrl-]
hypha add --tag staging                     # another one-shot command
hypha add --reusable -n homelab            # a 30-day reusable key
hypha keys                                 # list saved keys
hypha keys --revoke e_xxx                 # kill a leaked key
hypha history
hypha cancel x_abc123                       # kill a running command
hypha audit                                 # who did what, newest first
hypha audit --action exec --host web01
hypha cp ./nginx.conf /etc/nginx/nginx.conf # atomic file push
hypha push root@10.0.0.5 -t prod            # install over your own ssh
```

`hypha push` is the one-click story that keeps the hub out of the credential
business: it mints a one-shot token, builds an install command, and runs it
through *your* ssh. Your keys never leave your machine, so the hub cannot become
a vault that unlocks every box you own.

Two things about that command are deliberate:

- it downloads the installer to a file and then runs it, instead of
  `curl ... | sudo sh`. A pipeline reports the *last* command's status, so an
  unreachable hub means `sh` reads an empty stdin, exits 0, and the install looks
  successful on a machine where nothing happened. Piping is how a 130-second hang
  got reported as "installed, but the host never showed up".
- it carries `--connect-timeout 10`, so "the target cannot reach the hub" is a
  ten-second answer rather than a two-minute one.

**Reinstalling mints a new identity, and the hub retires the old record.** The
newest enrollment claiming a hostname wins (`host.replace` in the audit trail).
Without that, every reinstall leaves a ghost host that is offline forever. The
consequence to know about: two agents claiming the same hostname cannot coexist,
so test satellites should pass `enroll --name smoke-$$` — which is exactly what
`scripts/smoke.sh` does.

## How it works

| | |
|---|---|
| **agent → hub** | one WebSocket, JSON frames, agent initiated |
| **auth** | enrol token → agent generates ed25519 keypair → hub challenges with a nonce on every connect |
| **exec** | hub sends `exec{id,command}`, agent runs `$SHELL -lc` in its own session, streams 8 KiB chunks with a monotonic `seq`, then an exit frame |
| **pty** | raw pty on the agent (`portable-pty`), base64 frames relayed by the hub. A session outlives its WebSocket: the hub keeps a 256 KiB scrollback and reattaches with replay, so a reload lands you back in the same shell (`pty_ttl_sec`, default 600) |
| **streaming** | cli/panel read an SSE feed backed by a per-exec replay buffer — reconnect and you get everything you missed. Finished runs are also written to SQLite, so history survives a hub restart |
| **files** | base64 in JSON, written by the agent as temp file + fsync + rename: a failed push never leaves a half-written file |
| **metrics** | the agent samples /proc and statvfs every 30s and sends cpu/mem/disk/procs/uptime with its heartbeat |
| **install URLs** | commands are minted from the origin the caller actually used, falling back to `public_url` — hit the hub over a tailnet IP or a MagicDNS name and the command matches. Loopback origins fall back, because `127.0.0.1` means nothing to the machine you are installing on |
| **host death** | hub fails every in-flight job for that host immediately; nothing hangs |

The agent never listens on a port and never talks to anything except the hub.

## Security, honestly

- **The agent runs as root.** That is the design: it is your hands on the box. A
  hub compromise is root on the whole fleet. Keep the hub patched, keep TLS on,
  and don't put it on a coffee-shop wifi.
- Auth is per-agent ed25519 keys (a shared secret would let one agent impersonate
  another — we don't do that).
- **There is no panel/CLI auth.** The network is the boundary. Agent auth is
  still real (ed25519), so a random host cannot join your fleet.
- The hub deliberately cannot read ssh passwords or private keys, even when they
  sit in another system's inventory. Installing a satellite is done from *your*
  machine (`hypha push`), not from the hub.
- Every state change lands in the `audit` table (enroll, tags, delete, token
  revoke, exec, cancel, file push, shell open) and is readable at
  `GET /api/audit` or `hypha audit`. It is the only answer to "who ran what on
  which box last week".
- **Keep an out-of-band way in** (ssh, cloud console). A tool that becomes your
  only door and then breaks is worse than ssh.

## The panel lives in another repository

This repository is the backend: API, agent rendezvous, installer channel, SQLite,
audit. There is no frontend code here, and that is deliberate.

The panel is [**mycelium**](https://github.com/JoeWest9966/mycelium) — React +
TypeScript, generated from the contract in `api/openapi.yaml`. It can be
deployed anywhere that can reach the API, and it ships on its own schedule.

Three things make that split safe rather than merely tidy:

**1. `api/openapi.yaml` is the single source of truth.** Every panel-facing
route, request body, response shape and stream framing lives there. It is
OpenAPI 3.1, so the TypeScript client is generated, not hand-written.

**2. There is a version gate.** `GET /api/meta` returns `api_version`. A panel
refuses to render against a version it does not know, instead of half-working
against a contract it was not built for. Breaking changes bump it; added fields
do not.

**3. The router and the spec cannot drift.** `scripts/check-api-contract.sh`
fails when a route is served but undocumented, or documented but not served.

```
browser ──► nginx :5522 ──┬─► /var/www/mycelium   React build, static
                          └─► 127.0.0.1:5599      hyphad: /api, /agent, /install.sh, /dl
```

`hyphad` runs API-only when no panel bundle is present — that is a supported
deployment shape, not a broken install:

```json
{ "listen": "127.0.0.1:5599", "public_url": "https://hub.example.com" }
```

It still serves the installer endpoints (`/install.sh`, `/dl/{name}`) because
those are how a satellite gets onto a machine, not part of the panel. See
`dist/nginx-hypha.conf` for the routing, SSE/WebSocket streaming settings and the
`Cache-Control` split (`index.html` no-store, hashed assets immutable). If you
fork `mycelium` and want the single-binary story back, point `static_dir` at its
`dist/` — the hub will serve it from anywhere.

## Release builds are static, and that is not negotiable

`cargo build --release` on any modern distribution produces a binary that needs
that distribution's glibc. Ship it and the first older machine answers with
`weak version 'GLIBC_2.39' not found` — and then keeps running, which is worse
than crashing, because it looks like it worked. It happened once; that machine
enrolled, wrote a systemd unit and went offline on the next restart.

So: `scripts/build.sh` targets `-unknown-linux-musl` with `musl-gcc` (`.cargo/config.toml`
sets the linker), `scripts/check-binaries.sh` greps every file under `bin/` for
`GLIBC_` symbols and runs `--version` on it, and `dist/install.sh` refuses to
install a binary that will not execute — before it burns the one-shot token and
writes a service that cannot start.

Upgrading a satellite *through* the agent needs care: the agent runs in
`hypha-sat.service`'s cgroup, so `systemctl stop` kills your upgrade script too,
`setsid` included. Either run the upgrade from outside (ssh, or `hypha push`), or
escape the cgroup with
`systemd-run --unit=hypha-upgrade --collect sh -c '...'`.

## Tests

```sh
scripts/check-api-contract.sh # router ↔ api/openapi.yaml, fails on drift
scripts/check-binaries.sh     # every shipped binary is static and runs
scripts/smoke.sh              # hub + local satellite: enroll, exec, exit codes, pty,
                              # file push, metrics, cancel, shell reattach, audit,
                              # and exec output surviving a hub restart. Enrolls
                              # under a throwaway name so it cannot retire a real
                              # host on the same box (pass --name in enroll for the
                              # same reason in your own tests)
```

The panel's own tests live in the mycelium repository.

## Status

Working today: enroll, fleet list, fan-out exec with live output and durable
history, cancellable runs, browser and CLI shells with reattach, tags, audit
trail, host removal, atomic file push, host metrics, install-over-ssh from your
own machine.

Not there yet: scheduled jobs, broadcast to a tag with per-host arguments, log
shipping, Windows/macOS agents.

## Layout

```
crates/proto   wire protocol (one file, read this first)
crates/hub     hyphad  — api, agent rendezvous, installer channel, sqlite
crates/agent   hypha-sat — the satellite
crates/cli     hypha   — the cli
api/           openapi.yaml — the contract the panel is built against
dist/          install.sh, units, nginx config
scripts/       build.sh (static musl), smoke.sh, check-api-contract.sh
```

No frontend. The panel is [mycelium](https://github.com/JoeWest9966/mycelium).

## License

MIT.
