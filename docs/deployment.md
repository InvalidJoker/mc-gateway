# Deployment

Run one gateway on **every node** that hosts customer servers, with one config
per node.

## How it works on a node

```text
player ──▶ node:30123
             │
             │ nftables: port in `ports`?
             ├── no ──────────────────────────────▶ customer container (as always)
             │
             └── yes ──▶ mc-gateway ──▶ customer container
                           │               (sees the player's IP)
                           │
                           ├─ status ping:  fetch the server's answer,
                           │                replace line 2, pass it back
                           ├─ server down:  answer with the offline MOTD / kick
                           └─ anything else: pass the bytes through untouched
```

On start the gateway installs an nftables table `mcgateway`, a routing rule
(table `6767`) and an `INPUT` rule accepting its own mark. It generates all of
them from its config, so the port range and the firewall cannot drift apart.

## Requirements

- Linux with nftables (any current distribution)
- `ip`, `nft`, `iptables`/`ip6tables` — included in the container image
- Customer servers as Docker containers with published ports (Pterodactyl,
  Pelican) or as plain processes on the node

## Configuration

[`deploy/config.yaml`](../deploy/config.yaml) is the template. The keys that
matter:

| key | meaning |
|---|---|
| `ports` | your customer port range(s), e.g. `["25565-25665", "30000-31000"]` |
| `motd.line2` | your line. `&` colour codes and `&#rrggbb` work |
| `motd.line1` | optional: replaces the first line too (normally the customer's) |
| `offline.motd` | server list text for a server that does not answer |
| `offline.version` | shown in red where the ping bars would be |
| `offline.kick` | disconnect message for a player joining an offline server; `null` just closes |
| `offline.enabled` | `false` makes an offline server's port behave like a closed port |
| `listen` / `listen_v6` | internal hand-over addresses; change only if port 25500 is taken or inside `ports`. `listen_v6: null` turns IPv6 off |
| `timeouts.drain` | how long open connections get when the gateway stops |
| `metrics` | Prometheus endpoint, off by default |
| `log.client_ip` | log player addresses, off by default |

Check a config without starting anything:

```bash
docker run --rm -v "$PWD/config.yaml:/c.yaml:ro" --entrypoint mc-gateway \
    ghcr.io/invalidjoker/mc-gateway:latest --config /c.yaml --check
```

## Installation

### Option A: Docker (recommended)

```bash
mkdir -p /opt/mc-gateway && cd /opt/mc-gateway
curl -fsSLO https://raw.githubusercontent.com/InvalidJoker/mc-gateway/HEAD/deploy/compose.yml
curl -fsSLO https://raw.githubusercontent.com/InvalidJoker/mc-gateway/HEAD/deploy/config.yaml
nano config.yaml                      # ports and your line
docker compose up -d
docker compose logs -f
```

[`deploy/compose.yml`](../deploy/compose.yml) sets `network_mode: host` and
`cap_add: [NET_ADMIN]`; both are required. The container installs the rules as
root and then runs as an unprivileged user holding only that one capability.

Images are published to `ghcr.io/invalidjoker/mc-gateway` for `linux/amd64`
and `linux/arm64`:

| tag | |
|---|---|
| `latest`, `1`, `1.2`, `1.2.3` | releases (git tags `v1.2.3`) |
| `edge` | the latest commit on the default branch |
| `sha-abc1234` | a specific commit |

### Option B: systemd

```bash
cargo build --release
sudo install -m 755 target/release/mc-gateway /usr/local/bin/
sudo install -D -m 644 deploy/config.yaml /etc/mc-gateway/config.yaml
sudo useradd --system --no-create-home --shell /usr/sbin/nologin mc-gateway
sudo install -m 644 deploy/mc-gateway.service /etc/systemd/system/
sudo systemctl enable --now mc-gateway
```

The unit uses the host's own firewall tools — the better choice when the host
still runs iptables-legacy (the image ships iptables-nft).

## Checking it works

1. **Log** — on start you see `network setup installed` and one `intercepting`
   line each for IPv4 and IPv6.
2. **Ping from outside** — add a customer server to your Minecraft server list.
   The second line must be yours. Stop that server: it must show as offline.
3. **Metrics**, if enabled:

   ```bash
   curl -s localhost:9100/metrics | grep mc_gateway
   ```

   | metric | |
   |---|---|
   | `mc_gateway_status_requests_total` | status pings recognised |
   | `mc_gateway_motd_rewrites_total` | of those, answered with your line |
   | `mc_gateway_offline_answers_total` | offline MOTDs and kicks for unreachable servers |
   | `mc_gateway_connections_active` | connections currently running through the gateway |
   | `mc_gateway_server_unreachable_total` | connections whose server did not answer |

   If `status_requests` and `motd_rewrites` stay apart, some servers answer with
   something that cannot be rewritten; they get their original answer.

## Operating it

**Change your line or the offline texts** — edit the config, then reload. No
connection is dropped:

```bash
docker compose kill -s HUP          # Docker
systemctl reload mc-gateway         # systemd
```

**Change the ports** — edit the config and **restart**. The firewall rules are
generated on start.

**Restarts and updates** — ⚠️ players currently connected through the gateway
are disconnected when it stops; their connection runs through the process.
While it is down, new connections go straight to the servers. Update outside
peak hours:

```bash
docker compose pull && docker compose up -d
```

**Load** — all traffic of intercepted connections runs through the gateway:
two sockets and some CPU for copying per player. `LimitNOFILE` in the systemd
unit is set accordingly.

## What a customer's server sees

The same player address it would see without the gateway, on a different
source port:

| server | IPv4 player | IPv6 player |
|---|---|---|
| container, Docker network without IPv6 (default) | player's IP | Docker's bridge address, e.g. `172.17.0.1` |
| container, Docker network with IPv6 | player's IP | player's IPv6 |
| plain process on the node | player's IP | player's IPv6 |

The bridge address in the first row is Docker's doing (`docker-proxy`), not the
gateway's. Enabling IPv6 on the customers' Docker network removes it.

## Failure behaviour

| situation | result |
|---|---|
| customer server stopped, or no server on the port | offline MOTD in the server list, kick message on join |
| gateway stopped or crashed | connections go straight to the servers, without your line |
| server answers with something unreadable | its original answer passes through |
| not Minecraft on the port (RCON, HTTP, …) | passed through untouched; closed if the server is down |
| host firewall with `INPUT DROP` (ufw) | works — the gateway adds its own rule |
| node without IPv6 | only IPv4 is intercepted, with a warning in the log |

Note that every port in `ports` without a server answers with the offline MOTD,
so a port scan of the range shows Minecraft servers on all of them. Set
`offline.enabled: false` if that matters to you.

## Host firewall

Intercepted connections are delivered to the node itself, so they cross the
`INPUT` chain, which published Docker ports otherwise never do. On start the
gateway therefore adds to `iptables` and `ip6tables`:

```text
-A INPUT -m mark --mark 0x6d67 -j ACCEPT
```

Opening the port range would not help: after Docker's DNAT the packet carries
the container's internal port. Tested with an `INPUT DROP` policy. **firewalld**
keeps its own rules, where this has no effect; the same exception (fwmark
`0x6d67` in input) has to be added there by hand — untested.

## Removing it

```bash
docker compose down
docker run --rm --network host --cap-add NET_ADMIN --entrypoint sh \
    ghcr.io/invalidjoker/mc-gateway:latest -c 'mc-gateway --print-network-teardown | sh'
```

With systemd:

```bash
sudo systemctl disable --now mc-gateway
mc-gateway --print-network-teardown | sudo sh
```

Rules left behind after stopping are harmless: without a running gateway they
do not match.

## Troubleshooting

| symptom | cause |
|---|---|
| `cannot set IP_TRANSPARENT … Operation not permitted` | `NET_ADMIN` is missing (`cap_add` or `AmbientCapabilities`) |
| no line, but connections work | gateway not running, or the port is not in `ports` |
| players cannot connect once the gateway runs | a firewall drops `INPUT` and the gateway's rule has no effect (firewalld, or iptables-legacy with the container → use systemd) |
| every port shows offline | the servers are unreachable from the node itself — check `docker ps` and the published ports |
| `listen port … lies inside ports` | move `listen` to a port outside the range |

Inspect the active rules: `nft list table inet mcgateway`. See what would be
installed without changing anything: `mc-gateway --print-network-setup`.
