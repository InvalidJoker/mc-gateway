# mc-gateway

A Minecraft edge gateway: one public port in front of many backends.

It routes on the hostname from the handshake, answers server list pings itself,
and hands backends the real client IP — while understanding as little of the
Minecraft protocol as it possibly can. Everything after the handshake is
forwarded byte for byte, so compression, encryption and every modloader's own
traffic pass through untouched.

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
                 │  status / MOTD       │
                 │  routing             │
                 │  IP forwarding       │
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
| **Central MOTD** | The gateway answers status pings. The server list stays up while backends restart, and `protocol: auto` echoes the client's own version number so no client ever sees the red "incompatible" cross. Clients back to 1.6 get a legacy reply. |
| **Client IP** | PROXY protocol v2 for software that reads it (Paper, Velocity), Linux TPROXY for software that cannot (vanilla, Fabric, Forge, NeoForge, Quilt). |
| **Groups** | Several servers per target with round-robin, least-connections or failover selection, weights and per-server connection caps. |
| **Health checks** | TCP or full status ping, with rise/fall thresholds. Dead backends leave the pool instead of swallowing joins. |
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
    forwarding: proxy_protocol_v2
```

`config.example.yaml` documents every option.

## Layout

```text
crates/
  protocol/     VarInt, framing, handshake, status, legacy ping
  config/       configuration model, parsing, validation
  routing/      host matching, backend registry, selection, health checks
  forwarding/   PROXY protocol v1/v2, Linux TPROXY
  metrics/      counters and a dependency-free Prometheus exporter
  gateway/      listeners, sessions, MOTD, limits, reload — the binary
test-infra/     docker compose network with real server software
deploy/         systemd unit, nftables rules for TPROXY and backends
docs/           architecture, forwarding, operations, compatibility
```

The split is the point: transport, protocol, routing and IP forwarding do not
know about each other, so a new modloader or Minecraft version is a
configuration change rather than a rewrite.

## Documentation

- [Architecture](docs/architecture.md) — how a connection flows through it, and why the layers are separate
- [Forwarding](docs/forwarding.md) — the three IP forwarding modes, when each applies, and the full TPROXY setup
- [Operations](docs/operations.md) — deployment, metrics, reload, tuning, security
- [Compatibility](docs/compatibility.md) — what is verified against what
- [Roadmap](docs/roadmap.md) — what is not built yet

## Tests

```bash
cargo test --workspace          # unit and integration tests
cargo clippy --workspace --all-targets
```

Integration tests start a real gateway on real sockets, drive it with a
hand-written Minecraft client and assert on what fake backends actually
received — including the exact bytes of the PROXY header and the replayed
handshake.

`test-infra/smoke.py` does the same against the compiled binary:

```bash
python3 test-infra/smoke.py ./target/release/mc-gateway --config smoke.yaml
```
