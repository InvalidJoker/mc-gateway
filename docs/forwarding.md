# Client IP forwarding

Without help, a backend behind a proxy sees the proxy's address for every
player. Bans, per-IP limits, geo lookups and anti-cheat all break. There are
three ways to fix that, and which one applies depends entirely on what the
backend software can read.

| mode | backend sees | needs | works with |
|---|---|---|---|
| `none` | the gateway's IP | nothing | anything |
| `proxy_protocol_v2` | the real IP, via a header before the first Minecraft byte | backend support + config | Paper, Spigot, Folia, Velocity, BungeeCord, HAProxy-aware software |
| `transparent` | the real IP, at the TCP layer | Linux, `CAP_NET_ADMIN`, routing setup | **everything**, including vanilla and every modloader |

Set it per server, never globally:

```yaml
servers:
  paper-01:
    address: "10.10.1.10:25565"
    kind: paper
    forwarding: proxy_protocol_v2

  vanilla-01:
    address: "10.10.1.20:25565"
    kind: vanilla
    forwarding: transparent
```

Sending a PROXY header to something that cannot read it does not degrade
gracefully — the backend treats the header as a malformed packet and drops the
connection. The gateway warns at start-up when `kind` and `forwarding` are an
impossible pair.

## What is deliberately not here

Velocity's "modern" forwarding and BungeeCord's `\0`-suffixed handshake do more
than carry an IP: they carry an **authenticated identity** — the player's UUID
and their signed Mojang profile properties. A proxy may only assert those if it
performed the authentication itself.

This gateway never authenticates anyone. It does not decrypt, it does not talk
to Mojang's session server, and it does not hold a keypair. Emitting those
formats would mean fabricating an identity, and any backend accepting it would
be trivially spoofable by anyone who could reach it directly.

So the gateway forwards addresses only, and leaves identity to the component
that actually verified it:

```text
player ──▶ mc-gateway ──▶ Velocity ──▶ Paper
              │              │
        IP only, via    authenticates the player,
        PROXY v2        then forwards the real identity
                        using its own modern forwarding
```

That is the supported way to combine the two. Velocity reads the PROXY header
(`haproxy-protocol = true`), so it sees the player's real address, and its own
forwarding to Paper keeps working unchanged.

## PROXY protocol v2

The gateway writes a 28-byte binary header (IPv4; 52 for IPv6) before the
handshake. Health checks send the specification's `LOCAL` command instead, which
tells the backend "this connection is mine, not a player's" — so a
proxy-protocol-only listener accepts the probe without the gateway inventing a
fake client address.

### Backend configuration

**Paper / Spigot / Folia** — `spigot.yml`:

```yaml
settings:
  proxy-protocol: true
```

**Velocity** — `velocity.toml`:

```toml
haproxy-protocol = true
```

Both are all-or-nothing: once enabled, *every* connection must carry a header.
Make sure the backend is unreachable except through the gateway before you turn
it on, and keep the health check's forwarding mode matching the session's — the
gateway does this automatically.

### Inbound headers

If an L4 load balancer sits in front of the gateway, it can pass the client's
address on the same way:

```yaml
listeners:
  - name: public
    bind: "0.0.0.0:25565"
    proxy_protocol:
      enabled: true
      trusted: ["10.0.0.0/8"]   # mandatory
      required: false
```

`trusted` is not optional, and the gateway refuses to start without it. A header
is a *claim* about who is connecting; honouring one from an arbitrary peer would
let any player assert any source IP, which defeats every per-IP limit and
poisons the logs. Headers from outside `trusted` are ignored — the bytes are
then not a valid handshake, so the connection is dropped.

Both v1 (text) and v2 (binary) are accepted inbound; v2 is always used outbound.

## Linux TPROXY (`transparent`)

The gateway binds its outbound socket to the *player's* address before
connecting. The backend's `accept()` reports the real IP, with no protocol
support and no configuration on the backend at all.

The cost is host setup, because the backend now replies to the player's address
rather than the gateway's, and those replies have to come back:

```text
player 203.0.113.50
        │
        ▼
    mc-gateway ──── src=203.0.113.50 ───▶ backend 10.10.1.20
        ▲                                        │
        └──────── dst=203.0.113.50 ◀─────────────┘
              must be routed back to the gateway
```

Three things are required:

1. **`CAP_NET_ADMIN`** on the gateway process, to set `IP_TRANSPARENT`.
   The systemd unit in `deploy/systemd/` grants exactly this.
2. **Divert rules on the gateway**, so replies addressed to a player are
   delivered into the local socket instead of routed onward:
   `deploy/nftables/tproxy.nft` plus `deploy/nftables/tproxy-setup.sh`
   (an `ip rule` on a firewall mark, a `local default` route, `rp_filter=0`).
3. **A return path from the backends.** Either the gateway is the backends'
   default route, or policy routing on each backend sends traffic for player
   networks back to the gateway.

Point 3 is the one that decides whether this is practical for you. If the
backends are VMs on a network you control and the gateway is their router, it is
straightforward. If they are containers on a host you do not fully control, use
`proxy_protocol_v2` where you can and `none` where you cannot.

### Bringing it up

Do not switch everything at once. The failure modes are all "the connection
hangs", and they look identical:

```text
1. TPROXY ──▶ a plain TCP echo server      does the socket bind and connect?
2. TPROXY ──▶ vanilla                       does a real server see the right IP?
3. TPROXY ──▶ Paper                         confirm with a plugin that logs IPs
4. TPROXY ──▶ Velocity                      confirm it survives the second hop
```

A one-line Paper plugin, or simply `/list` plus the server log on join, is
enough to verify step 3. The gateway's own logs show which forwarding mode it
used for each session:

```text
INFO routing player client=203.0.113.50:51234 host=survival.example.net
     player=Notch protocol=767 version=1.21.x target=survival
     backend=survival-01 forwarding=transparent
```

### IPv6

A v4 player can be bound onto a v6 backend socket (as `::ffff:a.b.c.d`), but a
v6 player cannot be expressed on a v4 socket. If you serve IPv6 players, give
the backends IPv6 addresses too, or use `proxy_protocol_v2` for them.

## Platform behaviour

`forwarding: transparent` needs Linux. On other platforms the config still
*validates* — so you can check a production config from a laptop — but the
gateway refuses to start with a clear message rather than failing on the first
player who lands on that backend.
