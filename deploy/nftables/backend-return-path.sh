#!/bin/sh
# Backend side of the TPROXY return path.
#
# A backend behind a transparent gateway receives connections whose source is a
# player's address. Its replies would normally follow the default route — past
# the gateway, which then never sees them, and the session hangs.
#
# This sends replies for exactly those connections back via the gateway, and
# leaves everything else alone: the backend's own outbound traffic (downloads,
# databases, other servers) keeps using its normal route.
#
#   GATEWAY_IP=10.77.0.2 LOCAL_SUBNET=10.77.0.0/24 backend-return-path.sh
#
# Run once per network namespace, before the server accepts players. Safe to
# run again.
set -eu

: "${GATEWAY_IP:?set GATEWAY_IP to the gateway's address on this network}"
: "${LOCAL_SUBNET:?set LOCAL_SUBNET to this network's subnet}"

MARK=1
TABLE=100

nft -f - <<NFT
table ip mcreturn
flush table ip mcreturn

table ip mcreturn {
    chain inbound {
        type filter hook prerouting priority mangle; policy accept;

        # A new inbound connection from outside the local subnet can only have
        # been opened transparently by the gateway on a player's behalf.
        # Remember that on the connection itself.
        iifname != "lo" meta l4proto tcp ct state new ip saddr != $LOCAL_SUBNET ct mark set $MARK
    }

    chain outbound {
        # A route hook, so the kernel re-routes the packet after the mark.
        type route hook output priority mangle; policy accept;
        ct mark $MARK meta mark set $MARK
    }
}
NFT

ip rule show | grep -Eq "fwmark 0x0*$MARK lookup $TABLE" \
    || ip rule add fwmark "$MARK" lookup "$TABLE"
ip route replace default via "$GATEWAY_IP" table "$TABLE"

echo "backend-return-path: replies to gateway-opened connections go via $GATEWAY_IP"
