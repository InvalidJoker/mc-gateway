#!/bin/sh
# Dev lab: a simulated hosting node you can point a real Minecraft client at.
#
# A privileged docker:dind container plays the node. Inside it run customer
# servers as containers with published ports, like a panel would create them,
# and the gateway in front of them with host networking. Ports 30000-30002 of
# the node are published on this machine's localhost.
#
#   dev/lab.sh up             start the lab (PAPER=1 for a real Paper on 30002)
#   dev/lab.sh ping 30000     status ping from this machine
#   dev/lab.sh gateway        rebuild and restart the gateway after code changes
#   dev/lab.sh reload         re-read dev/gateway.yaml (edit the MOTD line live)
#   dev/lab.sh stop-gateway   stop the gateway, to see fail-open
#   dev/lab.sh logs           follow the gateway log
#   dev/lab.sh down           remove everything
set -eu
cd "$(dirname "$0")"

NET=mc-gateway-lab
NODE=mc-gateway-lab-node

node() { docker exec "$NODE" "$@"; }

start_gateway() {
    docker build -q -t mc-gateway:dev .. >/dev/null
    docker save mc-gateway:dev | docker exec -i "$NODE" docker load >/dev/null
    node docker rm -f gateway >/dev/null 2>&1 || true
    # The whole dev directory is mounted, not the single file, so edits made by
    # editors that replace the file on save are still seen on reload.
    node docker run -d --name gateway --network host --cap-add NET_ADMIN \
        -v /lab:/etc/mc-gateway:ro mc-gateway:dev --config /etc/mc-gateway/gateway.yaml >/dev/null
    echo "gateway started"
}

customer() { # name, port, motd
    node docker run -d --name "$1" -p "$2:25565" -v /lab:/lab:ro \
        python:3-alpine python -u /lab/mc_server.py "$3" >/dev/null
}

case "${1:-help}" in
up)
    docker network create --ipv6 --subnet fd00:6d63:9::/64 "$NET" >/dev/null
    docker run -d --privileged --name "$NODE" --network "$NET" \
        -p 127.0.0.1:30000-30002:30000-30002 -v "$PWD:/lab:ro" docker:dind >/dev/null
    printf 'waiting for the node'
    until node docker info >/dev/null 2>&1; do printf .; sleep 2; done; echo

    customer survival 30000 "Survival SMP"
    customer creative 30001 "Creative World"
    if [ "${PAPER:-0}" = 1 ]; then
        echo "starting a real Paper server on 30002 (the first start downloads it)"
        node docker run -d --name paper -p 30002:25565 \
            -e EULA=TRUE -e TYPE=PAPER -e ONLINE_MODE=FALSE -e MEMORY=1G \
            itzg/minecraft-server:latest >/dev/null
    else
        customer skyblock 30002 "Skyblock"
    fi
    start_gateway

    cat <<MSG

Lab is up. In Minecraft, add these servers:
  localhost:30000   Survival SMP
  localhost:30001   Creative World
  localhost:30002   ${PAPER:+real Paper server}${PAPER:-Skyblock}

The fake servers answer the server list only; joining needs PAPER=1.
Edit motd.line2 in dev/gateway.yaml, then: dev/lab.sh reload
MSG
    ;;
gateway) start_gateway ;;
reload) node docker kill -s HUP gateway >/dev/null && echo "config reloaded" ;;
stop-gateway) node docker stop gateway >/dev/null && echo "gateway stopped: pings now show the servers' own MOTD" ;;
logs) node docker logs -f gateway ;;
ping) python3 mc_player.py 127.0.0.1 "${2:?port}" status ;;
down)
    docker rm -f "$NODE" >/dev/null 2>&1 || true
    docker network rm "$NET" >/dev/null 2>&1 || true
    echo "lab removed"
    ;;
*) sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//' ;;
esac
