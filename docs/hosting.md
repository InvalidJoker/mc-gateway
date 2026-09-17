# Hosting nodes: a line in every customer's MOTD

On a node that runs customers' Minecraft servers, mc-gateway sits in front of a
port range and does one visible thing: it replaces the second line of each
server's MOTD with yours. Everything else about the customer's server — their
first line, player count, version, icon, logins, gameplay — is theirs and passes
through untouched.

```text
                     player dials node:30123
                               │
              ┌────────────────┼─────────────────┐
              │ node           ▼                 │
              │   nftables: port in range? ──────┼── no ──▶ untouched
              │                │ yes             │
              │                ▼                 │
              │           mc-gateway             │
              │     status ping? ── no ──▶ pipe ─┼──▶ customer container
              │          │ yes                   │    (sees the player's IP)
              │          ▼                       │
              │   ask the server, swap line 2 ───┼──▶ customer container
              └──────────────────────────────────┘
```

## Setup

The configuration is short:

```yaml
intercept:
  listen: "127.0.0.1:25500"
  ports: ["25565-25665"]        # your panel's allocation range

motd:
  line2: "&7Hosted by &bexample.net"
```

`deploy/node/config.yaml` is a complete example.

### With systemd (recommended)

```bash
cargo build --release
sudo install -m 755 target/release/mc-gateway /usr/local/bin/
sudo install -d /etc/mc-gateway
sudo install -m 644 deploy/node/config.yaml /etc/mc-gateway/config.yaml
sudo useradd --system --no-create-home --shell /usr/sbin/nologin mc-gateway
sudo install -m 644 deploy/systemd/mc-gateway.service /etc/systemd/system/
sudo systemctl enable --now mc-gateway
```

The unit installs the firewall and routing rules before start, with the host's
own `nft`, `ip` and `iptables`, and runs the gateway unprivileged with only
`CAP_NET_ADMIN`.

### With Docker

```bash
cd deploy/node
docker compose up -d --build
```

Host networking and `cap_add: [NET_ADMIN]` are required, and set in that file.
The image ships `iptables-nft`; on a host whose firewall still uses
iptables-legacy, use systemd instead.

## What gets the ad, and what does not

| connection | what happens |
|---|---|
| status ping (server list) | server answers, line 2 replaced, rest untouched |
| login / play | piped through byte for byte |
| pre-1.7 ping | piped through, no ad |
| anything that is not Minecraft (RCON, HTTP, a query tool…) | piped through |
| a server that speaks first (SSH, FTP…) | piped through the moment it speaks |
| a port outside `intercept.ports` | never reaches the gateway |
| IPv6 | never reaches the gateway |

The decision is made on the first bytes. As soon as they cannot be a status ping,
or the client says nothing within `timeouts.handshake`, the connection is
passed through. Every error while looking at a status response forwards the
server's original bytes. The worst case for a customer is a ping without your
line — never a broken connection.

A customer's server sees the player's real IP address, on a different source
port than the player used.

## Failure behaviour

All of these are checked by `test-infra/intercept-check/run.sh` against a real
Docker node:

| situation | result |
|---|---|
| gateway running | ad on status pings, everything else untouched |
| gateway stopped or crashed, rules still installed | **fail-open**: traffic goes straight to the servers, without the ad |
| customer's server stopped | the connection is closed, as a closed port would be |
| host firewall dropping INPUT (ufw) | works — see below |
| gateway restarted | rules are replaced, not duplicated |
| teardown | rules removed, traffic untouched |

Fail-open works because the redirect rule only matches when the gateway is
actually listening. Stopping the gateway is always safe for customers.

## Host firewalls

Connections the gateway handles are delivered to the node itself, so they cross
the `INPUT` chain — which Docker-published ports normally never do. A firewall
that drops there, such as ufw's default policy, would silently stop every
intercepted connection. Opening the port range does not help: after Docker's
DNAT the packet carries the container's internal port.

The setup therefore inserts one rule into `INPUT` that accepts packets carrying
the gateway's own mark, and the teardown removes it again:

```text
-A INPUT -m mark --mark 0x6d67 -j ACCEPT
```

That has been tested with an iptables `INPUT DROP` policy. firewalld keeps its
rules in its own nftables table, where this rule has no effect; the requirement
is the same — accept fwmark `0x6d67` in input — but it has to be added through
firewalld, and that has not been tested.

## Operating it

**Changing the ad** — edit `motd.line2`, then `systemctl reload mc-gateway`
(or `docker compose kill -s HUP`). No connection is dropped.

**Changing the port range or listen address** — restart. The rules are
generated from the config on start.

**Metrics** — `mc_gateway_status_requests_total{kind="intercept"}` counts status
pings seen, `mc_gateway_motd_rewrites_total` those that got the ad. A gap
between the two means servers are answering with something the gateway could
not rewrite.

**Uninstalling** — stop the gateway, then remove the rules:

```bash
mc-gateway --config /etc/mc-gateway/config.yaml --print-network-teardown | sudo sh
```

**Inspecting the rules** — `--print-network-setup` prints exactly what is
installed, without installing anything.

## Limitations

- IPv4 only. IPv6 connections are not intercepted and get no ad.
- A stopped server's port accepts the connection and then closes it, instead of
  refusing it outright. The server list shows it as unreachable either way.
- The same line goes to every server on the node. There is no per-customer
  exemption (for example, a paid tier without the ad) yet.
- Tested with Docker containers with published ports, which is what Pterodactyl
  and Pelican Wings create — not yet on a production node running Wings itself.
