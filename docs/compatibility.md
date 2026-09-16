# Compatibility

## What the gateway actually touches

Only the handshake, the status exchange and the legacy ping. Those have been
stable since 1.7, which is why the compatibility surface is small:

| protocol element | last changed |
|---|---|
| VarInt framing | never |
| handshake packet | `next_state = 3` (transfer) added in 1.20.5 — supported |
| status response | only the `description` field is touched, and only when MOTD lines are configured |
| ping/pong | never — forwarded |
| legacy ping (≤ 1.6) | superseded; detected and forwarded to a backend, never answered here |

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
| a backend's status response passing through untouched | `tests/status.rs` |
| only the configured MOTD line changing, with counts, sample, version and favicon intact | `tests/status.rs`, `crates/gateway/src/motd.rs` |
| chat components flattened to legacy `§` codes, including hex colours and inherited styles | `crates/protocol/src/chat.rs` |
| an unparseable status response forwarded rather than dropped | `tests/status.rs` |
| the offline MOTD when a route has no reachable backend | `tests/status.rs` |
| legacy 1.6 ping forwarded verbatim | `tests/status.rs` |
| handshake replayed to the backend byte for byte, with nothing prepended | `tests/routing.rs`, `tests/forwarding.rs` |
| inbound PROXY v1/v2 parsing, and untrusted headers ignored | `crates/forwarding/src/inbound.rs`, `tests/forwarding.rs` |
| the Prometheus endpoint, scraped over HTTP | `tests/metrics.rs` |
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

Fill it in by running each combination rather than assuming it. Three things are
worth checking per cell:

1. a client can join and stay joined (proves the pipe and the replay)
2. the server list shows the backend's own MOTD with your second line
3. on Linux, the backend logs the player's real IP (proves TPROXY)

`test-infra/docker-compose.yml` brings up vanilla, Paper, Velocity, Fabric and
NeoForge behind the gateway for exactly this. Point a client at `localhost:25565`
and use the hostnames in `test-infra/gateway/config.yaml` to pick a backend.

## Forwarding support by software

Every backend is reached the same way, and none of them need to know:

| software | works on Linux (TPROXY) | works elsewhere | needs configuring |
|---|---|---|---|
| Vanilla | yes, with the real IP | yes, gateway's IP | nothing |
| Paper / Spigot / Folia | yes, with the real IP | yes, gateway's IP | `proxy-protocol: false` |
| Velocity | yes, with the real IP | yes, gateway's IP | `haproxy-protocol = false` |
| BungeeCord | yes, with the real IP | yes, gateway's IP | its proxy-protocol option off |
| Fabric / Forge / NeoForge / Quilt | yes, with the real IP | yes, gateway's IP | nothing |

The only thing to check is that a backend is *not* expecting a PROXY header: the
gateway sends none, and a listener waiting for one will hang.

`kind` in the config is metadata. It drives logs and metrics labels and changes
no behaviour.
