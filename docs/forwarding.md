# Client IP forwarding

Without help, a backend behind a proxy sees the proxy's address for every
player. Bans, per-IP limits, geo lookups and anti-cheat all break.

There is no setting for this. The gateway picks by platform:

| platform | how it connects | backend sees |
|---|---|---|
| Linux | TPROXY: the outbound socket is bound to the player's own address | the player's real IP |
| everything else | an ordinary `connect()` | the gateway |

Nothing is ever added to the byte stream. The first bytes a backend receives are
the client's handshake, on every platform — which is why this works for vanilla,
Fabric, Forge, NeoForge and Quilt, none of which can read a proxy header, with
no configuration on their side at all.

## What is deliberately not here

**No PROXY protocol towards backends.** It would only help the subset of
software that can read it, and it needs matching configuration on both ends —
exactly the kind of per-server setting that goes wrong at 300 servers. TPROXY
covers everything, uniformly.

If a backend is configured to expect a header (`proxy-protocol: true` in Paper's
`spigot.yml`, `haproxy-protocol = true` in `velocity.toml`), **turn it off**. It
will otherwise wait for a header that never arrives.

**No Velocity or BungeeCord forwarding.** Those carry an *authenticated
identity* — the player's UUID and their signed Mojang profile — and a proxy may
only assert those if it performed the authentication itself. This gateway never
does: it does not decrypt, does not talk to Mojang's session server, and holds
no keypair. Emitting those formats would mean fabricating an identity, and any
backend accepting it would be trivially spoofable by anyone who could reach it
directly.

Putting Velocity behind the gateway is the supported way to combine the two:

```text
player ──▶ mc-gateway ──▶ Velocity ──▶ Paper
              │              │
       transparent, so   authenticates the player,
       Velocity sees     then forwards the real identity
       the real IP       with its own modern forwarding
```

## Linux TPROXY

```text
player 203.0.113.50
        │
        ▼
    mc-gateway ──── src=203.0.113.50 ───▶ backend 10.10.1.20
        ▲                                        │
        └──────── dst=203.0.113.50 ◀─────────────┘
              must be routed back to the gateway
```

Three things are required, and all three live outside this process:

1. **`CAP_NET_ADMIN`** on the gateway process, to set `IP_TRANSPARENT`.

   ```bash
   sudo setcap cap_net_admin+ep /usr/local/bin/mc-gateway
   ```

   The systemd unit in `deploy/systemd/` grants it instead
   (`AmbientCapabilities=CAP_NET_ADMIN`); a container needs
   `cap_add: [NET_ADMIN]`. Without it the gateway refuses to start and prints
   these options — it does not silently fall back, because a silent fallback
   would mean every backend quietly losing the client IP.

2. **Divert rules on the gateway**, so replies addressed to a player are
   delivered into the local socket instead of routed onward:

   ```bash
   sudo deploy/nftables/tproxy-setup.sh
   ```

   That adds an `ip rule` on a firewall mark, a `local default` route in its own
   table, turns off strict reverse-path filtering (the gateway sends packets
   whose source is not its own) and loads `deploy/nftables/tproxy.nft`.

3. **A return path from the backends.** Replies to gateway-opened connections
   must go back via the gateway. Either the gateway is the backends' default
   route, or each backend runs `deploy/nftables/backend-return-path.sh`:

   ```bash
   sudo GATEWAY_IP=10.77.0.2 LOCAL_SUBNET=10.77.0.0/24 deploy/nftables/backend-return-path.sh
   ```

   It connection-marks new inbound connections whose source is outside the local
   subnet — which only the gateway can open — and routes just those replies via
   the gateway. The backend's own traffic keeps its normal route.

### In Docker

Containers are Linux on every host — Docker Desktop and OrbStack on macOS
included — so this always applies. `test-infra/docker-compose.yml` is a working
example, and `test-infra/tproxy-check/run.sh` verifies it end to end. Four things
are needed, and each one's absence makes sessions hang:

| requirement | why |
|---|---|
| `cap_add: [NET_ADMIN]` on the gateway | The image's entrypoint sets up the gateway side of the return path, then drops to an unprivileged user that keeps this one capability as an ambient capability. A `USER` line cannot do that: a non-root user gets no effective capabilities from `cap_add`. |
| masquerading off on the network players arrive on | Docker masquerades any packet from a network's subnet that leaves via another bridge. A transparent connection is exactly that, so the backend would see a bridge address. `com.docker.network.bridge.enable_ip_masquerade: "false"`. |
| that network is the gateway's first interface | Docker orders interfaces by network name, and desktop port forwarders connect to the first one. If it is the backend network, host traffic arrives from an address inside the backend subnet, which cannot be told apart from a real neighbour. |
| a return-path sidecar per backend | `network_mode: service:<backend>` running `backend-return-path.sh`. |

Also keep the gateway's fixed address out of Docker's dynamic pool
(`ip_range`), or a backend that starts first can take it.

From the Mac host, a desktop VM presents your connection as the players
network's bridge address (e.g. `192.168.167.1`) — that is what the backend logs,
and it is still the address the gateway saw rather than the gateway's own. A
client container on that network, or a remote player, shows its real address.

### Bringing it up

Do not switch everything at once. The failure modes all look like "the
connection hangs", and they look identical:

```text
1. TPROXY ──▶ a plain TCP echo server      does the socket bind and connect?
2. TPROXY ──▶ vanilla                       does a real server see the right IP?
3. TPROXY ──▶ Paper                         confirm with a plugin that logs IPs
4. TPROXY ──▶ Velocity                      confirm it survives the second hop
```

For step 3, `/list` plus the server log on join is enough. The gateway's own log
names the mode it is using at start-up:

```text
INFO backend connections use this mode on this platform forwarding=transparent
```

### IPv6

A v4 player can be bound onto a v6 backend socket (as `::ffff:a.b.c.d`), but a
v6 player cannot be expressed on a v4 socket. If you serve IPv6 players, give
the backends IPv6 addresses too.

## Inbound PROXY protocol

This is the other direction, and it *is* configurable: if an L4 load balancer
sits in front of the gateway, it can pass the client's address along.

```yaml
listeners:
  - name: public
    bind: "0.0.0.0:25565"
    proxy_protocol:
      enabled: true
      trusted: ["10.0.0.0/8"]   # mandatory
      required: false
```

Both v1 (text) and v2 (binary) are accepted.

`trusted` is not optional, and the gateway refuses to start without it. A header
is a *claim* about who is connecting; honouring one from an arbitrary peer would
let any player assert any source IP. That matters more here than in most
proxies, because on Linux the address the gateway believes is the address it
then binds — a spoofed header would make the gateway open a connection as
someone else. Headers from outside `trusted` are ignored; the bytes are then not
a valid handshake, so the connection is dropped.
