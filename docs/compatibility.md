# Compatibility

## What the gateway actually touches

Only the handshake, the status exchange and the legacy ping. Those have been
stable since 1.7, which is why the compatibility surface is small:

| protocol element | last changed |
|---|---|
| VarInt framing | never |
| handshake packet | `next_state = 3` (transfer) added in 1.20.5 — supported |
| status request/response | shape stable; `enforcesSecureChat` added in 1.19.1 — supported |
| ping/pong | never |
| legacy ping (≤ 1.6) | superseded, still answered |

Everything else is forwarded unread. A new Minecraft version does not need a
gateway update: an unknown protocol number logs as `protocol=<n>` and is proxied
like any other.

## Verified here

Covered by automated tests in this repository:

| behaviour | test |
|---|---|
| VarInt against the protocol's reference vectors, incl. negatives | `crates/protocol/src/varint.rs` |
| handshake parsing, `\0FML\0` / `\0FML2\0` / `\0FML3\0` / `\0FORGE`, BungeeCord suffixes, SRV trailing dots | `crates/protocol/src/handshake.rs`, `tests/routing.rs` |
| `next_state` 1 / 2 / 3 | `crates/protocol/src/handshake.rs` |
| status JSON shape and protocol echoing for 1.8, 1.12 and 1.21 clients | `tests/status.rs` |
| legacy 1.6 ping, field layout and delimiter injection | `crates/protocol/src/legacy.rs`, `tests/status.rs` |
| handshake replayed to the backend byte for byte | `tests/routing.rs` |
| PROXY v2 header layout against the specification | `crates/forwarding/src/proxy_protocol.rs` |
| PROXY v2 end to end, with the client's real address and port | `tests/forwarding.rs` |
| `LOCAL` command for health checks and proxied pings | `tests/forwarding.rs` |
| untrusted inbound headers ignored | `tests/forwarding.rs` |
| the compiled binary, end to end | `test-infra/smoke.py` |

## Not verified here

The matrix below is what the design supports. Nothing in it has been run
against real server software in this repository, and that distinction matters:
the protocol work is tested, the *integration* is not.

| Minecraft | Vanilla | Paper | Fabric | Forge | NeoForge | Velocity |
|---|---|---|---|---|---|---|
| 1.7.10 | ? | – | – | ? | – | ? |
| 1.8.9 | ? | ? | – | ? | – | ? |
| 1.12.2 | ? | ? | ? | ? | – | ? |
| 1.16.5 | ? | ? | ? | ? | – | ? |
| 1.18.2 | ? | ? | ? | ? | – | ? |
| 1.20.1 | ? | ? | ? | ? | ? | ? |
| 1.21.x | ? | ? | ? | ? | ? | ? |

Fill it in by running each combination rather than assuming it. Two things are
worth checking per cell:

1. a client can join and stay joined (proves the pipe and the replay)
2. the backend logs the player's real IP (proves the forwarding mode)

`test-infra/docker-compose.yml` brings up vanilla, Paper, Velocity, Fabric and
NeoForge behind the gateway for exactly this. Point a client at `localhost:25565`
and use the hostnames in `test-infra/gateway/config.yaml` to pick a backend.

## Forwarding support by software

| software | `proxy_protocol_v2` | `transparent` | notes |
|---|---|---|---|
| Vanilla | no | yes | no proxy support of any kind |
| Paper / Spigot / Folia | yes | yes | `settings.proxy-protocol: true` in `spigot.yml` |
| Velocity | yes | yes | `haproxy-protocol = true` |
| BungeeCord | yes | yes | via `proxy_protocol` in its config |
| Fabric | no | yes | unless a mod adds it |
| Forge / NeoForge / Quilt | no | yes | unless a mod adds it |

`kind` in the config is metadata: it drives logs, metrics labels and the
start-up warning when `kind` and `forwarding` are an impossible pair. It never
selects a forwarding mode on its own — that stays explicit, because the software
running on a port is not something a proxy should guess.
