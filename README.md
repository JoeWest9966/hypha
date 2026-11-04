# hypha

**One binary on every server. One panel. No more ssh.**

hypha is a star-topology fleet manager: a hub you run somewhere reachable, and a
tiny agent (`hypha-sat`) on every machine. The agent dials **out** to the hub, so
there are no inbound ports to open, no keys to distribute per host, and no VPN.
Everything you do — run a command on 40 boxes, open a shell, watch the output
stream — goes over that one long-lived connection.

```
                    ┌──────────────────────┐
   you ──browser──► │        hyphad        │ ◄── your CLI
                    │  panel · api · audit │
                    └──────────┬───────────┘
                 outbound ws   │   outbound ws
              ┌────────────────┼────────────────┐
           ┌──▼───┐         ┌──▼───┐         ┌──▼───┐
           │ sat  │         │ sat  │         │ sat  │
           └──────┘         └──────┘         └──────┘
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

### 2. run the hub (on the box that will hold the panel)

```sh
sudo mkdir -p /var/lib/hypha
sudo ./target/release/hyphad --listen 0.0.0.0:5522 --public-url https://hub.example.com
```

**There is no login.** The hub assumes it is reachable only from a private
network (Tailscale, WireGuard, loopback). Anybody who can reach port 5522 can
run commands as root on every enrolled host — so don't expose it to the
internet. If you need a public hub, put an auth proxy in front of it.

For a public hub, put TLS in front of it. Caddy is one line (`dist/Caddyfile`):

```
hub.example.com {
    reverse_proxy 127.0.0.1:5522
}
```

`hyphad` itself speaks plain HTTP on purpose — a reverse proxy gives you certs,
HSTS, and access logs for free.

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
```

## How it works

| | |
|---|---|
| **agent → hub** | one WebSocket, JSON frames, agent initiated |
| **auth** | enrol token → agent generates ed25519 keypair → hub challenges with a nonce on every connect |
| **exec** | hub sends `exec{id,command}`, agent runs `$SHELL -lc` in its own session, streams 8 KiB chunks with a monotonic `seq`, then an exit frame |
| **pty** | raw pty on the agent (`portable-pty`), base64 frames relayed by the hub |
| **streaming** | cli/panel read an SSE feed backed by a per-exec replay buffer — reconnect and you get everything you missed |
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
- `audit` table records enrollments and every command. Tail it.
- **Keep an out-of-band way in** (ssh, cloud console). A tool that becomes your
  only door and then breaks is worse than ssh.

## Panel and API are separate projects

The panel is being extracted into its own repository (**mycelium**), because it
needs a real build step (React + TypeScript) and it ships on its own schedule.
The hub is API-only from here on, and `api/openapi.yaml` is the contract between
them.

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

## Tests

```sh
scripts/check-api-contract.sh # router ↔ api/openapi.yaml, fails on drift
scripts/smoke.sh              # hub + local satellite: enroll, exec, exit codes, pty, audit
cd panel-test && npm i && npm test   # legacy panel driven in jsdom (deleted once mycelium lands)
```

## Status

Early. Working today: enroll, fleet list, fan-out exec with live output, browser
and CLI shells, tags, audit, host removal. Not there yet: file transfer, session
reattach by id after a browser close, metrics, scheduled jobs, Windows/macOS
agents.

## Layout

```
crates/proto   wire protocol (one file, read this first)
crates/hub     hyphad  — panel, api, agent rendezvous, sqlite
crates/agent   hypha-sat — the satellite
crates/cli     hypha   — the cli
dist/          install.sh, units, proxy config
scripts/       build.sh (static musl), smoke.sh
panel-test/    headless panel test (jsdom)
api/           openapi.yaml — the contract the panel is built against
crates/hub/static  legacy panel (vanilla js); mycelium replaces it
```

## License

MIT.
