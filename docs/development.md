# Development

## Layout

A single Rust package:

```text
src/
  main.rs         CLI: start, --check, --print-network-setup/-teardown, signals
  server.rs       start the listeners, reload the config (SIGHUP), stop cleanly
  intercept.rs    one intercepted connection: status ping, join, or pass through
  motd.rs         replace line 1/2 in a status response; offline documents
  chat.rs         chat components ⇄ § colour codes
  protocol.rs     VarInt, packets, handshake — only what happens before login
  transparent.rs  TPROXY sockets: the listener, and connecting "as the player"
  netsetup.rs     generates the nftables / routing / firewall scripts from the config
  config.rs       config file, defaults, validation
  observe.rs      Prometheus metrics
tests/            integration tests for intercept.rs
dev/              dev lab and end-to-end test
deploy/           config template, container entrypoint, compose, systemd
.github/          CI and image publishing
```

## How a connection flows

```text
1. nftables hands new connections on `ports` to 127.0.0.1:25500 / [::1]:25500
2. intercept::run accepts them; the socket's local address is the original
   destination (after Docker's DNAT: the customer's container)
3. transparent::connect connects there, bound to the player's address
     └─ fails → answer_offline: offline MOTD, kick message, or close
4. intercept::sniff reads the client's first bytes:
     handshake with next_state=status + status request  → status
     handshake with next_state=login/transfer           → join
     anything else, or the server speaks first          → pass through
5. status: read the server's answer, motd::rewrite_status, pass it on
6. then, in every case: copy_bidirectional until either side closes
```

The rule throughout: **when in doubt, pass through**. Nothing that goes wrong
while looking at the traffic may break a customer's connection; at worst the
line is missing.

### The firewall rules

`netsetup.rs` generates everything. Two conntrack marks keep the directions
apart:

| mark | on | purpose |
|---|---|---|
| `0x6d63` | an intercepted player connection | the player's next packets → gateway |
| `0x6d65` | a connection gateway → server | the server's replies → gateway |
| `0x6d64` | the gateway's socket (`SO_MARK`) | sets `0x6d65` in `output` |
| `0x6d67` | a packet | routing table `6767`: deliver locally |

Server replies are caught in two places: `prerouting` for containers behind a
bridge, and `output` for plain processes and `docker-proxy`, which Docker uses
for IPv6 when a container has no IPv6 address.

The TPROXY rule only matches when a socket is listening. With the gateway gone,
traffic flows normally — that is the fail-open.

## Build and test

```bash
cargo build
cargo test                    # unit and integration tests; run on macOS too
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The integration tests in `tests/intercept.rs` call `intercept::handle` directly
with a fake server's address, so the logic runs without TPROXY — on a Mac too.
The fake server waits for the handshake **and** the status request before it
answers, like a real one; not doing so once hid a real bug.

Check the Linux-only code from a Mac:

```bash
rustup target add x86_64-unknown-linux-musl   # once
cargo check --target x86_64-unknown-linux-musl
```

## Dev lab

`dev/lab.sh` builds a simulated hosting node you can point a **real Minecraft
client** at. It only needs Docker (OrbStack or Docker Desktop are fine).

```text
your machine                 Docker
                           ┌──────────────────────────────────────────┐
Minecraft ─ localhost:35565│ lab node (docker:dind)                   │
            localhost:35566│   mc-gateway       (node's host network) │
            localhost:35567│   survival :30000 ─┐                     │
                           │   creative :30001 ─┼─ customer containers│
                           │   skyblock :30002 ─┘  with port bindings │
                           └──────────────────────────────────────────┘
```

```bash
dev/lab.sh up               # start everything
dev/lab.sh ping 35565       # status ping from your machine
dev/lab.sh reload           # after editing dev/gateway.yaml
dev/lab.sh gateway          # after code changes: rebuild and restart
dev/lab.sh stop survival    # see the offline MOTD; `start survival` brings it back
dev/lab.sh stop-gateway     # see the fail-open
dev/lab.sh logs             # gateway log
dev/lab.sh down             # remove everything
```

Add `localhost:35565` to `35567` as servers in Minecraft. The three fake servers
(`dev/mc_server.py`) only answer the server list. To join, start with
`PAPER=1 dev/lab.sh up` — `localhost:35567` is then a real Paper server (its
first start downloads it, which takes a while).

If the ports are taken, `LAB_PORT=40000 dev/lab.sh up` moves them.

To try MOTD changes live: edit `motd.line2` or `offline` in `dev/gateway.yaml`,
run `dev/lab.sh reload`, refresh the server list.

## End-to-end test

```bash
dev/check.sh
```

Builds the image, starts its own lab node, and checks at the kernel level, over
IPv4 **and** IPv6, for three kinds of customer server (container without IPv6,
container with IPv6, plain process):

- the line is replaced, player count and first line stay
- the server sees the same address on login as without the gateway
- ports outside the range are untouched
- a port without a server, and a customer container that is stopped, show the
  offline MOTD and kick message; a restarted server gets its own MOTD back
- fail-open with the gateway stopped
- restart without duplicated rules
- host firewall with `INPUT DROP` on both families
- teardown removes everything

It takes a few minutes and cleans up after itself. CI runs it on every push;
run it locally before changing `intercept.rs`, `transparent.rs` or
`netsetup.rs` — the unit tests cannot check the firewall rules.

## CI and releases

`.github/workflows/ci.yml` runs on every push and pull request. The image is
only published once the tests and the end-to-end test pass.

| job | |
|---|---|
| `test` | `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` |
| `e2e` | `dev/check.sh` on a GitHub runner |
| `image` | builds the image for `linux/amd64` and `linux/arm64`; pushes to GHCR from the default branch (`edge`) and from `v*` tags |

To release: `git tag v1.2.3 && git push --tags`. That publishes
`ghcr.io/invalidjoker/mc-gateway:1.2.3`, `1.2`, `1` and `latest`.

The Dockerfile cross-compiles on the build machine's own architecture instead
of emulating the target, so both architectures build in minutes.

## Open points

- **Per-customer exemptions** — every server on a node gets the same line. A
  plan without the ad would need a list of ports without the rewrite, reloadable
  with `SIGHUP`.
- **Restarts disconnect players** — their connection runs through the process.
- **firewalld**, **Wings on a real node** and the **systemd unit** are untested
  (the Docker path is tested).
