#!/bin/sh
# End-to-end check of port interception on a real Docker node.
#
# A privileged docker:dind container plays the hosting node: customer servers
# are containers with published ports, exactly like Pterodactyl or Pelican
# Wings, and the gateway runs on the node with host networking. A second
# container plays a player on the internet.
#
# Nothing here touches the host's own firewall or Docker daemon.
set -eu
cd "$(dirname "$0")"

NET=mc-intercept-check
NODE=mc-intercept-node
PLAYER=mc-intercept-player
failures=0

cleanup() {
    docker rm -f "$NODE" "$PLAYER" >/dev/null 2>&1 || true
    docker network rm "$NET" >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup

node() { docker exec "$NODE" "$@"; }
play() { docker exec "$PLAYER" python /lab/mc_player.py "$NODE" "$1" "$2"; }

expect() { # description, output, pattern that must match
    if printf '%s' "$2" | grep -q -- "$3"; then
        echo "PASS  $1"
    else
        echo "FAIL  $1"; echo "      got: $2"; failures=$((failures + 1))
    fi
}
reject() { # description, output, pattern that must not match
    if printf '%s' "$2" | grep -q -- "$3"; then
        echo "FAIL  $1"; echo "      got: $2"; failures=$((failures + 1))
    else
        echo "PASS  $1"
    fi
}

echo "building the gateway image..."
docker build -q -t mc-gateway:dev ../.. >/dev/null

docker network create "$NET" >/dev/null
docker run -d --privileged --name "$NODE" --network "$NET" -v "$PWD:/lab:ro" docker:dind >/dev/null
docker run -d --name "$PLAYER" --network "$NET" -v "$PWD:/lab:ro" python:3-alpine sleep infinity >/dev/null

echo "waiting for the node's Docker daemon..."
i=0; until node docker info >/dev/null 2>&1; do i=$((i + 1)); [ $i -lt 60 ] || exit 1; sleep 2; done

docker save mc-gateway:dev | docker exec -i "$NODE" docker load >/dev/null
node docker run -d --name customer -p 30123:25565 -v /lab:/lab:ro python:3-alpine python -u /lab/mc_server.py CUSTOMER >/dev/null
node docker run -d --name outside -p 30500:25565 -v /lab:/lab:ro python:3-alpine python -u /lab/mc_server.py OUTSIDE >/dev/null

echo "waiting for the customer servers..."
i=0; until play 30123 status | grep -q CUSTOMER; do i=$((i + 1)); [ $i -lt 60 ] || exit 1; sleep 2; done

start_gateway() {
    node docker rm -f gateway >/dev/null 2>&1 || true
    node docker run -d --name gateway --network host --cap-add NET_ADMIN \
        -v /lab/gateway.yaml:/etc/mc-gateway/config.yaml:ro mc-gateway:dev >/dev/null
    i=0; until node docker logs gateway 2>&1 | grep -q intercepting; do i=$((i + 1)); [ $i -lt 30 ] || exit 1; sleep 1; done
}
player_ip=$(docker exec "$PLAYER" hostname -i | awk '{print $1}')

echo
echo "== gateway running =="
start_gateway
expect "status ping in range gets the ad"        "$(play 30123 status)" "Hosted by"
expect "  ...and keeps the customer's line one"  "$(play 30123 status)" "CUSTOMER"
expect "  ...and the customer's player count"    "$(play 30123 status)" "players 3/20"
expect "login reaches the server with the player's IP" "$(play 30123 login)" "LOGIN-SEEN-FROM $player_ip"
reject "a port outside the range is untouched"   "$(play 30500 status)" "Hosted by"
expect "a port with no server stays closed"      "$(play 30124 status)" "Error"

echo
echo "== gateway stopped, rules left in place =="
node docker stop gateway >/dev/null
reject "fail-open: no ad"                         "$(play 30123 status)" "Hosted by"
expect "fail-open: the server is still reachable" "$(play 30123 status)" "CUSTOMER"
expect "fail-open: logins still work"             "$(play 30123 login)" "LOGIN-SEEN-FROM $player_ip"

echo
echo "== gateway restarted =="
node docker start gateway >/dev/null; sleep 2
expect "the ad is back"                     "$(play 30123 status)" "Hosted by"
expect "rules were replaced, not duplicated" "$(node sh -c 'ip rule | grep -c 6767')" "^1$"

echo
echo "== host firewall that drops INPUT (like ufw) =="
node sh -c 'iptables -A INPUT -i lo -j ACCEPT
            iptables -A INPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
            iptables -P INPUT DROP'
node docker restart gateway >/dev/null; sleep 3
expect "the gateway opened INPUT for its own mark" "$(node iptables -S INPUT)" "mark 0x6d67 -j ACCEPT"
expect "status ping still gets the ad"             "$(play 30123 status)" "Hosted by"
expect "login still gets through"                  "$(play 30123 login)" "LOGIN-SEEN-FROM $player_ip"

echo
echo "== teardown =="
node docker stop gateway >/dev/null
# Run inside the gateway image, which has nft and ip; the node image has not.
node docker run --rm --network host --cap-add NET_ADMIN --entrypoint sh \
    -v /lab/gateway.yaml:/c.yaml:ro mc-gateway:dev \
    -c 'mc-gateway --config /c.yaml --print-network-teardown 2>/dev/null | sh'
# The node image has no nft of its own; the gateway image does.
tables=$(node docker run --rm --network host --cap-add NET_ADMIN --entrypoint nft mc-gateway:dev list tables 2>&1)
expect "the node's firewall is readable"  "$tables" "table ip"
reject "rules are gone"                   "$tables" "mcgateway"
reject "traffic is back to untouched" "$(play 30123 status)" "Hosted by"
reject "the INPUT rule is gone"       "$(node iptables -S INPUT)" "0x6d67"

echo
if [ "$failures" -eq 0 ]; then echo "all checks passed"; else echo "$failures check(s) failed"; exit 1; fi
