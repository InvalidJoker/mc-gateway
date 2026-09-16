# mc-gateway

A Minecraft edge gateway: one public port in front of many backends.

It routes on the hostname from the handshake and then gets out of the way. A
status ping is forwarded to the backend and the backend's own answer is returned
— version, player count, sample and favicon included — with at most two lines of
the MOTD swapped out on the way back. Everything after the handshake is copied
byte for byte, so compression, encryption and every modloader's own traffic pass
through untouched.

```text
                         INTERNET
                            │
                    mc.example.net:25565
                            │
                            ▼
                 ┌──────────────────────┐
                 │     MC-GATEWAY       │
                 │                      │
                 │  L4 TCP proxy        │
                 │  handshake parser    │
                 │  routing             │
                 │  MOTD line rewriting │
                 │  limits              │
                 │  health checks       │
                 └──────────┬───────────┘
                            │  private network
          ┌─────────────────┼─────────────────┐
          ▼                 ▼                 ▼
       Vanilla           Velocity          Modded
                            │
                     ┌──────┼──────┐
                     ▼      ▼      ▼
                   Paper  Fabric  NeoForge
```

## What it does

| | |
|---|---|
| **Routing** | Exact hosts, wildcards (`*.play.example.net`), a catch-all and a default. Forge/NeoForge markers and SRV trailing dots are stripped before matching, so modded clients route like everyone else. |
| **MOTD** | The backend's status response is passed through. Only the MOTD lines you name are replaced — usually just the second one, for network branding. Everything else stays real. |
| **Client IP** | On Linux every backend connection is transparent (TPROXY): the backend's `accept()` reports the player's address, with no support needed on its side, for vanilla and modloaders alike. On other platforms the gateway connects normally. |
| **Groups** | Several servers per target with round-robin, least-connections or failover selection, weights and per-server connection caps. |
| **Health checks** | TCP or a full status ping, with rise/fall thresholds. Dead backends leave the pool instead of swallowing joins. |
| **Limits** | Global and per-IP connection caps, a per-IP token bucket, a handshake byte budget and timeouts on every phase. |
| **Observability** | Prometheus metrics, structured logs (text or JSON), and a `log.client_ip: false` switch for logging no personal data. |
| **Reload** | `SIGHUP` swaps routes, servers, MOTD and limits without dropping a session. |

## Quick start

```bash
cargo build --release
cp config.example.yaml config.yaml
# edit config.yaml
./target/release/mc-gateway --config config.yaml
```

**On Linux**, grant the capability transparent connections need, or the gateway
refuses to start and tells you this:

```bash
sudo setcap cap_net_admin+ep ./target/release/mc-gateway
```

The return path also has to be set up — see [forwarding](docs/forwarding.md).
`deploy/` has the systemd unit and nftables rules for it.

Check a config without starting anything:

```bash
./target/release/mc-gateway --config config.yaml --check
```

A minimal config is three blocks:

```yaml
listeners:
  - name: public
    bind: "0.0.0.0:25565"

routing:
  default: survival
  rules:
    - host: "survival.example.net"
      target: survival

servers:
  survival-01:
    address: "10.10.1.10:25565"
    group: survival
    kind: paper
```

Add a line of branding without touching anything else the backend reports:

```yaml
motd:
  line2: "&7survival &8• &7creative &8• &7modded"
```

`config.example.yaml` documents every option.

## Layout

```text
crates/
  protocol/     VarInt, framing, handshake, status, chat components
  config/       configuration model, parsing, validation
  routing/      host matching, backend registry, selection, health checks
  forwarding/   Linux TPROXY, plain TCP, inbound PROXY protocol
  gateway/      listeners, sessions, MOTD rewriting, limits, reload — the binary
test-infra/     docker compose network with real server software, smoke test
deploy/         systemd unit, nftables rules for TPROXY and backends
docs/           architecture, forwarding, operations, compatibility
```

The split is the point: transport, protocol, routing and IP forwarding do not
know about each other, so a new modloader or Minecraft version is a
configuration change rather than a rewrite.

The protocol code is deliberately small, and everything around it comes from a
library: [`governor`] for rate limiting, [`ipnet`] for CIDRs,
[`humantime-serde`] for durations, [`proxy-header`] for the PROXY protocol,
[`craftping`] for status pings, [`socket2`] for TPROXY sockets,
[`tokio-io-timeout`] with `copy_bidirectional` for the pipe, and
[`metrics`]/[`metrics-exporter-prometheus`] for the scrape endpoint.

[`governor`]: https://crates.io/crates/governor
[`ipnet`]: https://crates.io/crates/ipnet
[`humantime-serde`]: https://crates.io/crates/humantime-serde
[`proxy-header`]: https://crates.io/crates/proxy-header
[`craftping`]: https://crates.io/crates/craftping
[`socket2`]: https://crates.io/crates/socket2
[`tokio-io-timeout`]: https://crates.io/crates/tokio-io-timeout
[`metrics`]: https://crates.io/crates/metrics
[`metrics-exporter-prometheus`]: https://crates.io/crates/metrics-exporter-prometheus

## Documentation

- [Architecture](docs/architecture.md) — how a connection flows through it, and why the layers are separate
- [Forwarding](docs/forwarding.md) — TPROXY, what it needs, and how to bring it up
- [Operations](docs/operations.md) — deployment, metrics, reload, tuning, security
- [Compatibility](docs/compatibility.md) — what is verified against what
- [Roadmap](docs/roadmap.md) — what is not built yet

## Tests

```bash
cargo test --workspace
cargo clippy --workspace --all-targets
```

Integration tests start a real gateway on real sockets, drive it with a
hand-written Minecraft client and assert on what fake backends actually
received — including that a status response arrives with the backend's own
player count and only the configured MOTD line changed.

`test-infra/smoke.py` does the same against the compiled binary:

```bash
python3 test-infra/smoke.py ./target/release/mc-gateway --config test-infra/smoke.yaml
```
