#!/bin/sh
# End-to-end check of port interception on a real Docker node, IPv4 and IPv6.
#
# A privileged docker:dind container plays the hosting node, the gateway runs
# on it with host networking, and a second container plays a player reaching
# the node over both address families. Three kinds of customer server cover
# the three paths a connection can take on a node:
#
#   30123  container, IPv4-only bridge  IPv6 arrives through docker-proxy
#   30130  container with IPv6          both families through Docker's DNAT
#   30140  plain process on the node    no Docker in between at all
#
# The central assertion: whatever address a server sees for a player without
# the gateway, it sees exactly the same with it.
#
# Nothing here touches the host's own firewall or Docker daemon. Takes a few
# minutes; run it before every change to the network code is merged.
set -eu
cd "$(dirname "$0")"

NET=mc-intercept-check
NODE=mc-intercept-node
PLAYER=mc-intercept-player
PORTS="30123 30130 30140"
failures=0

cleanup() {
    docker rm -f "$NODE" "$PLAYER" >/dev/null 2>&1 || true
    docker network rm "$NET" >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup

node() { docker exec "$NODE" "$@"; }
play() { docker exec "$PLAYER" python /lab/mc_player.py "$1" "$2" "$3"; }

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
docker build -q -t mc-gateway:dev .. >/dev/null

docker network create --ipv6 --subnet fd00:6d63:1::/64 "$NET" >/dev/null
docker run -d --privileged --name "$NODE" --network "$NET" -v "$PWD:/lab:ro" docker:dind >/dev/null
docker run -d --name "$PLAYER" --network "$NET" -v "$PWD:/lab:ro" python:3-alpine sleep infinity >/dev/null

V4=$NODE
V6=$(docker inspect "$NODE" -f "{{(index .NetworkSettings.Networks \"$NET\").GlobalIPv6Address}}")

echo "waiting for the node's Docker daemon..."
i=0; until node docker info >/dev/null 2>&1; do i=$((i + 1)); [ $i -lt 60 ] || exit 1; sleep 2; done

docker save mc-gateway:dev | docker exec -i "$NODE" docker load >/dev/null
node docker network create --ipv6 --subnet fd00:6d63:3::/64 customers6 >/dev/null
node docker run -d --name proxied -p 30123:25565 -v /lab:/lab:ro \
    python:3-alpine python -u /lab/mc_server.py PROXIED >/dev/null
node docker run -d --name dualstack --network customers6 -p 30130:25565 -v /lab:/lab:ro \
    python:3-alpine python -u /lab/mc_server.py DUALSTACK >/dev/null
node docker run -d --name hostproc --network host -v /lab:/lab:ro \
    python:3-alpine python -u /lab/mc_server.py HOSTPROC 30140 >/dev/null
node docker run -d --name outside -p 30500:25565 -v /lab:/lab:ro \
    python:3-alpine python -u /lab/mc_server.py OUTSIDE >/dev/null

echo "waiting for the customer servers..."
for port in $PORTS 30500; do
    i=0; until play "$V4" "$port" status | grep -q players; do i=$((i + 1)); [ $i -lt 60 ] || exit 1; sleep 2; done
done

# What each server sees for a player without the gateway, per family.
baseline() { play "$1" "$2" login | sed 's/^port [0-9]*: //'; }
for port in $PORTS; do
    eval "seen4_$port=\"\$(baseline \$V4 $port)\""
    eval "seen6_$port=\"\$(baseline \$V6 $port)\""
done

check_family() { # label, address, variable prefix
    for port in $PORTS; do
        expect "$1 $port: status ping gets the ad"          "$(play "$2" "$port" status)" "Hosted by"
        expect "$1 $port:   ...and keeps the server's players" "$(play "$2" "$port" status)" "players 3/20"
        eval "want=\$$3_$port"
        expect "$1 $port: server sees the same address as without the gateway ($want)" \
            "$(play "$2" "$port" login)" "$want\$"
    done
}

start_gateway() {
    node docker rm -f gateway >/dev/null 2>&1 || true
    node docker run -d --name gateway --network host --cap-add NET_ADMIN \
        -v /lab/gateway.yaml:/etc/mc-gateway/config.yaml:ro mc-gateway:dev >/dev/null
    i=0; until node docker logs gateway 2>&1 | grep -q '\[::1\]:25500'; do i=$((i + 1)); [ $i -lt 30 ] || exit 1; sleep 1; done
}

echo
echo "== gateway running =="
start_gateway
check_family IPv4 "$V4" seen4
check_family IPv6 "$V6" seen6
reject "IPv4: a port outside the range is untouched" "$(play "$V4" 30500 status)" "Hosted by"
reject "IPv6: a port outside the range is untouched" "$(play "$V6" 30500 status)" "Hosted by"
expect "IPv4: a port with no server stays closed"    "$(play "$V4" 30124 status)" "Error"
expect "IPv6: a port with no server stays closed"    "$(play "$V6" 30124 status)" "Error"

echo
echo "== gateway stopped, rules left in place =="
node docker stop gateway >/dev/null
for addr in "$V4" "$V6"; do
    for port in $PORTS; do
        reject "fail-open [$addr]:$port: no ad"            "$(play "$addr" "$port" status)" "Hosted by"
        expect "fail-open [$addr]:$port: server reachable" "$(play "$addr" "$port" status)" "players 3/20"
    done
done

echo
echo "== gateway restarted =="
node docker start gateway >/dev/null; sleep 3
expect "IPv4: the ad is back" "$(play "$V4" 30123 status)" "Hosted by"
expect "IPv6: the ad is back" "$(play "$V6" 30123 status)" "Hosted by"
expect "IPv4 rules were replaced, not duplicated" "$(node sh -c 'ip rule | grep -c 6767')" "^1$"
expect "IPv6 rules were replaced, not duplicated" "$(node sh -c 'ip -6 rule | grep -c 6767')" "^1$"

echo
echo "== host firewall that drops INPUT on both families (like ufw) =="
node sh -c 'for t in iptables ip6tables; do
                $t -A INPUT -i lo -j ACCEPT
                $t -A INPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
                $t -P INPUT DROP
            done
            ip6tables -A INPUT -p ipv6-icmp -j ACCEPT'
node docker restart gateway >/dev/null; sleep 3
expect "IPv4 INPUT opened for the gateway's mark" "$(node iptables -S INPUT)" "mark 0x6d67 -j ACCEPT"
expect "IPv6 INPUT opened for the gateway's mark" "$(node ip6tables -S INPUT)" "mark 0x6d67 -j ACCEPT"
check_family IPv4 "$V4" seen4
check_family IPv6 "$V6" seen6

echo
echo "== teardown =="
node docker stop gateway >/dev/null
# Run inside the gateway image, which has nft and ip; the node image has not.
node docker run --rm --network host --cap-add NET_ADMIN --entrypoint sh \
    -v /lab/gateway.yaml:/c.yaml:ro mc-gateway:dev \
    -c 'mc-gateway --config /c.yaml --print-network-teardown 2>/dev/null | sh'
tables=$(node docker run --rm --network host --cap-add NET_ADMIN --entrypoint nft mc-gateway:dev list tables 2>&1)
expect "the node's firewall is readable" "$tables" "table ip"
reject "rules are gone"                  "$tables" "mcgateway"
reject "IPv4 INPUT rule is gone"         "$(node iptables -S INPUT)" "0x6d67"
reject "IPv6 INPUT rule is gone"         "$(node ip6tables -S INPUT)" "0x6d67"
reject "IPv6 routing rule is gone"       "$(node ip -6 rule)" "6767"
reject "IPv4 traffic is back to untouched" "$(play "$V4" 30123 status)" "Hosted by"
reject "IPv6 traffic is back to untouched" "$(play "$V6" 30123 status)" "Hosted by"

echo
if [ "$failures" -eq 0 ]; then echo "all checks passed"; else echo "$failures check(s) failed"; exit 1; fi
