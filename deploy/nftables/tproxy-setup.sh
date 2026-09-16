#!/bin/sh
# Routing and sysctl setup for `forwarding: transparent`.
#
# Run once at boot, before mc-gateway starts. Everything here is host state, not
# something the gateway can set for itself.
set -eu

MARK=1
TABLE=100

# 1. Packets marked by the nftables divert chain are looked up in their own
#    table, whose only route says "this is for me".
ip rule show | grep -q "fwmark $MARK lookup $TABLE" \
    || ip rule add fwmark "$MARK" lookup "$TABLE"
ip route show table "$TABLE" | grep -q "local default" \
    || ip route add local default dev lo table "$TABLE"

# The same for IPv6, if the gateway serves it.
if [ -d /proc/sys/net/ipv6 ]; then
    ip -6 rule show | grep -q "fwmark $MARK lookup $TABLE" \
        || ip -6 rule add fwmark "$MARK" lookup "$TABLE"
    ip -6 route show table "$TABLE" | grep -q "local default" \
        || ip -6 route add local default dev lo table "$TABLE"
fi

# 2. The gateway sends packets whose source address is not its own. Strict
#    reverse path filtering would drop the replies.
sysctl -w net.ipv4.conf.all.rp_filter=0
sysctl -w net.ipv4.conf.default.rp_filter=0

# 3. Forwarding, because the gateway is now on the path between players and
#    backends rather than being an endpoint.
sysctl -w net.ipv4.ip_forward=1

# 4. Load the divert rules.
nft -f "$(dirname "$0")/tproxy.nft"

echo "TPROXY return path configured (mark $MARK, table $TABLE)"
