# Roadmap

What exists today, and what does not.

## Built

| | |
|---|---|
| TCP proxy with idle, handshake, status, connect and drain timeouts | ✓ |
| Handshake parser (VarInt, framing, host normalisation, modloader markers) | ✓ |
| Host-based routing: exact, wildcard, catch-all, default, per-listener rules | ✓ |
| Central MOTD, per-route overrides, favicon, `protocol: auto`, offline MOTD | ✓ |
| Legacy (≤ 1.6) ping | ✓ |
| Health checks: TCP and status ping, rise/fall, spread scheduling | ✓ |
| Backend registry with groups, weights, round-robin / least-connections / failover | ✓ |
| PROXY protocol v2 outbound, v1 + v2 inbound behind a trust boundary | ✓ |
| Linux TPROXY transparent forwarding | ✓ (code and deployment rules; see Compatibility) |
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

### Load testing

Nothing here has been run under load. Worth measuring before it matters:

- 1 000 / 10 000 concurrent sessions
- connection churn (join/leave storms after a restart)
- many simultaneous status pings — the cheapest way to hurt a gateway
- a slow backend, a backend that accepts and never answers, a backend that
  disappears mid-session
- malformed and hostile pre-handshake traffic

The per-phase timeouts and byte budgets are all bounded by design, but "bounded"
and "measured" are different claims.

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
a documented failover procedure.

### Smaller things

- Player-count aggregation across gateway instances (today `players: sessions`
  counts one instance's sessions)
- `mc_gateway_session_seconds` as a histogram rather than a sum
- A drain mode that stops new sessions to one backend without removing it
- Status response caching when a proxied ping is under heavy load
