# mc-gateway

[![CI](https://github.com/InvalidJoker/mc-gateway/actions/workflows/ci.yml/badge.svg)](https://github.com/InvalidJoker/mc-gateway/actions/workflows/ci.yml)

Puts your line into the MOTD of every Minecraft server on a hosting node — and
is invisible otherwise.

```text
Server list, before:        after:
  Steve's SMP                 Steve's SMP
  whitelist on                Hosted by example.net
```

The customer's first line, player count, version and icon stay theirs. Logins,
gameplay and anything that is not a status ping pass through untouched, and the
customer's server sees the player's real IP. A stopped server is shown as
offline, with a message you choose. When the gateway is not running,
connections go straight to the servers.

## Configuration

```yaml
ports: ["25565-25665"]                  # your allocation range
motd:
  line2: "&7Hosted by &bexample.net"
offline:
  motd: "&cThis server is offline"
```

Every option, with defaults: [`deploy/config.yaml`](deploy/config.yaml).

## Run it

```bash
docker run -d --name mc-gateway --network host --cap-add NET_ADMIN \
    -v /etc/mc-gateway/config.yaml:/etc/mc-gateway/config.yaml:ro \
    ghcr.io/invalidjoker/mc-gateway:latest
```

## Documentation

- **[Deployment](docs/deployment.md)** — installing on a node, operating, removing
- **[Development](docs/development.md)** — code layout, tests, and a dev lab you can point a real Minecraft client at

```bash
cargo test
dev/lab.sh up     # a simulated node on localhost:35565
dev/check.sh      # full end-to-end test on a real Docker node
```
