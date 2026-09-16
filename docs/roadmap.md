# Roadmap

What exists today, and what does not.

## Built

| | |
|---|---|
| TCP proxy with idle, handshake, status, connect and drain timeouts | ✓ |
| Handshake parser (VarInt, framing, host normalisation, modloader markers) | ✓ |
| Host-based routing: exact, wildcard, catch-all, default, per-listener rules | ✓ |
| Status pass-through with per-line MOTD rewriting, and a per-route override | ✓ |
| Offline MOTD when a route has no reachable backend | ✓ |
| Pre-1.7 pings detected and forwarded | ✓ |
| Health checks: TCP and status ping, rise/fall, spread scheduling | ✓ |
| Backend registry with groups, weights, round-robin / least-connections / failover | ✓ |
| Linux TPROXY for every backend connection; plain TCP elsewhere | ✓ (code and deployment rules; see Compatibility) |
| Inbound PROXY protocol v1/v2 behind a trust boundary | ✓ |
| Per-IP and global limits, token bucket, handshake byte budget | ✓ |
| Prometheus metrics and structured logs | ✓ |
| `SIGHUP` reload with state carried over | ✓ |
| Graceful drain on shutdown | ✓ |
| systemd unit, nftables rules, docker compose test network | ✓ |

## Not built

### Verified compatibility matrix

The largest remaining gap. See [compatibility](compatibility.md): the protocol
handling is tested, the integration with real server software is not. This is
the next thing to do, and `test-infra/` exists for it.

The TPROXY return path in particular has only been exercised as code, not on a
real Linux network. That is the single riskiest untested thing in the project.

### Load testing

Nothing here has been run under load. Worth measuring before it matters:

- 1 000 / 10 000 concurrent sessions
- connection churn (join/leave storms after a restart)
- many simultaneous status pings — every one now opens a backend connection,
  which is a bigger cost than it was when the gateway answered them itself
- a slow backend, a backend that accepts and never answers, a backend that
  disappears mid-session
- malformed and hostile pre-handshake traffic

The per-phase timeouts and byte budgets are all bounded by design, but "bounded"
and "measured" are different claims.

### Status response caching

Each client ping currently becomes one backend connection. A popular address
being pinged by every server-list scraper on the internet turns into real load
on the backend. Caching the last response for a second or two, keyed by target,
would remove almost all of it — at the cost of player counts being up to that
stale.

### Control API

Editing YAML for 300 servers does not scale, and a self-registering backend is
the natural next step:

```text
POST   /servers          register
DELETE /servers/:id      deregister
PATCH  /servers/:id      drain, change weight
GET    /servers          list with health
```

The registry is already built and swapped atomically, so this is an API surface
plus a persistence choice, not a redesign. Authentication is the part to get
right.

### High availability

```text
                    Internet
                       │
                 ┌─────▼─────┐
                 │ L4 LB/VIP │
                 └─────┬─────┘
             ┌─────────┴─────────┐
             ▼                   ▼
        mc-gateway #1       mc-gateway #2
             └─────────┬─────────┘
                       ▼
                  backend network
```

Two instances with identical configs already work: nothing in the gateway is
stateful across connections. The load balancer distributes *new* TCP
connections; an established session stays on the instance that accepted it and
cannot be moved. Inbound PROXY protocol support is already there for the hop
from the load balancer.

What is missing is shared configuration — which is the control API above — and
a documented failover procedure. With TPROXY the return path has to reach the
*right* instance, which makes the routing setup more delicate than with a single
gateway.

### Smaller things

- A drain mode that stops new sessions to one backend without removing it
- `mc_gateway_bytes_total` counting aborted sessions too, which needs a counting
  wrapper around the copy loop
- Registry gauges pushed on health transitions instead of sampled every five
  seconds
- An opt-out from transparent forwarding on Linux, for development on a machine
  without the routing setup
