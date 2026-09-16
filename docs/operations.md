# Operations

## Running it

```bash
mc-gateway --config /etc/mc-gateway/config.yaml
```

| flag | |
|---|---|
| `--config <path>` | configuration file (also `MC_GATEWAY_CONFIG`) |
| `--check` | validate and exit — use this in CI and before a reload |
| `--dump` | print the config as the gateway understands it, defaults filled in |
| `--log-level <level>` | override `log.level`; `RUST_LOG` overrides both |

`deploy/systemd/mc-gateway.service` runs it as an unprivileged user with a tight
sandbox. It grants `CAP_NET_ADMIN` only because TPROXY needs it — drop that line
if no backend uses `forwarding: transparent`.

## Reloading

```bash
systemctl reload mc-gateway     # or: kill -HUP $(pidof mc-gateway)
```

Reload re-reads the config and swaps in new routes, servers, MOTD, messages and
limits. Open sessions keep running on the old snapshot until they end. Health
state and session counts carry over for servers whose address did not change.

A config that fails to parse or validate is rejected and the running one stays
in place — check with `--check` first anyway, so the failure happens before
production sees it.

Listener changes need a restart. The gateway says so in the log rather than
silently ignoring them.

## Metrics

```yaml
metrics:
  enabled: true
  bind: "127.0.0.1:9100"
```

There is no authentication on this endpoint, and the metrics describe your
topology — keep it off the public interface. `/metrics` is the scrape endpoint,
`/healthz` is a liveness probe.

| metric | type | labels |
|---|---|---|
| `mc_gateway_connections_total` | counter | |
| `mc_gateway_connections_active` | gauge | |
| `mc_gateway_connections_rejected_total` | counter | `reason` |
| `mc_gateway_handshake_errors_total` | counter | `kind` |
| `mc_gateway_status_requests_total` | counter | `source` = gateway / backend / legacy |
| `mc_gateway_login_attempts_total` | counter | |
| `mc_gateway_backend_connections_total` | counter | `backend` |
| `mc_gateway_backend_failures_total` | counter | `backend` |
| `mc_gateway_bytes_total` | counter | `direction` |
| `mc_gateway_sessions_completed_total` | counter | |
| `mc_gateway_session_seconds_total` | counter | |
| `mc_gateway_backends` / `_healthy` | gauge | |
| `mc_gateway_sessions_active` | gauge | |
| `mc_gateway_backend_up` | gauge | `backend`, `kind` |
| `mc_gateway_backend_sessions` | gauge | `backend` |
| `mc_gateway_backend_players` | gauge | `backend` |
| `mc_gateway_backend_ping_seconds` | gauge | `backend` |

`reason` values are worth alerting on individually: `rate_limited` and
`per_ip_limit` mean abuse or a misconfigured client; `no_route` means a DNS
record points at you that your config does not know about; `no_backend` and
`backend_error` mean players are being turned away.

Health gauges are read from the registry at scrape time rather than mirrored
into counters, so they cannot drift from reality.

## Logging

```yaml
log:
  level: info
  format: text     # or json
  client_ip: true
```

`client_ip: false` replaces client addresses with `redacted` in every log line.
The gateway keeps working, metrics keep counting, and no personal data reaches
the log files.

One line per routed session at `info`, plus one when it ends with its duration
and byte counts. Refusals and protocol errors are `debug`, so a port scanner
does not fill the disk. Backend state changes are `warn`/`info` transitions
only — never one line per health check.

## Limits and timeouts

```yaml
limits:
  max_connections: 20000          # 0 = unlimited
  max_connections_per_ip: 8
  connection_rate:
    burst: 30
    per: 60s
  max_handshake_bytes: 8192
  exempt: ["127.0.0.1/32", "10.0.0.0/8"]

timeouts:
  handshake: 5s
  status: 10s
  connect: 3s
  idle: 600s
  drain: 30s
```

`idle` is the one to think about. Mobile clients disappear without a FIN, and
without an idle timeout those sessions hold a backend slot forever. 600s is
conservative; lower it if you run tight per-server caps.

`exempt` waives the per-IP limits (not the global cap) for monitoring and your
own networks. Without it, a status-ping exporter scraping every server will hit
the rate limit.

## Health checks

```yaml
health:
  enabled: true
  method: status      # or tcp
  interval: 10s
  timeout: 2s
  rise: 2
  fall: 3
  start_healthy: true
```

`status` performs a real ping, which also yields the player counts used by
`motd.players: backends`. `tcp` only proves something is listening — use it for
backends that are slow to answer pings.

`start_healthy: true` means a gateway restart does not reject players during the
first check interval. Set it to `false` if you would rather refuse than risk
sending someone to a server that is still booting.

Checks are spread across the interval rather than fired together, so 300
backends do not get probed in the same millisecond.

## Capacity

Each session costs two file descriptors and two 32 KiB buffers. 10 000
concurrent players is roughly 640 MB of buffers and 20 000 descriptors, so:

```ini
LimitNOFILE=131072
```

The systemd unit sets this. Also raise the listener backlog if you expect
restart stampedes:

```yaml
listeners:
  - name: public
    bind: "0.0.0.0:25565"
    backlog: 4096
```

## Security checklist

- Backends reachable **only** from the gateway — `deploy/nftables/backend-firewall.nft`.
  Every limit here is worthless if players can connect to `10.10.1.10:25565` directly.
- `proxy_protocol.trusted` set to the actual upstream, never `0.0.0.0/0`.
- Metrics bound to a private address.
- The gateway running unprivileged, with `CAP_NET_ADMIN` only if TPROXY is in use.
- `max_connections_per_ip` and `connection_rate` left on.
- `--check` in CI, so a typo never reaches a reload.
