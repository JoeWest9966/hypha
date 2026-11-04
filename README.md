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

On first boot it prints a generated admin password **once**. Change it later:

```sh
hyphad --set-password 'something-long'
```

Put TLS in front of it. Caddy is one line (`dist/Caddyfile`):

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

It downloads `hypha-sat`, enrolls (single-use token, 24h), installs a systemd
unit, starts it. The host appears in the panel in about a second.

### 4. use it

```sh
hypha login --hub https://hub.example.com
hypha hosts
hypha run -- 'uptime'                       # every host
hypha run web01 db01 -- 'systemctl status nginx'
hypha run -t prod -- 'df -h /'
hypha sh web01                              # full tty, detach with Ctrl-]
hypha add --tag staging                     # another enroll command
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
- Panel/CLI auth is a single admin password → bearer token. There is no RBAC
  because there is no team. Add it when you have one.
- `audit` table records logins, enrollments, and every command. Tail it.
- **Keep an out-of-band way in** (ssh, cloud console). A tool that becomes your
  only door and then breaks is worse than ssh.

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
crates/hub/static  the panel (vanilla js + xterm.js, no build step)
```

## License

MIT.
