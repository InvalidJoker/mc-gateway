#!/bin/sh
# Gateway side of the TPROXY return path.
#
# The gateway opens backend connections from the player's address. The backend
# replies to that address, and those replies must be delivered into the
# gateway's socket rather than routed onward. This script makes that happen.
#
# Run once per network namespace before mc-gateway starts: at boot on a host,
# or from the container entrypoint. Safe to run again.
set -eu

MARK=1
TABLE=100
HERE=$(dirname "$0")

# 1. Marked packets are looked up in their own table, whose only route says
#    "this is for me". `ip rule show` prints the mark in hex.
ip rule show | grep -Eq "fwmark 0x0*$MARK lookup $TABLE" \
    || ip rule add fwmark "$MARK" lookup "$TABLE"
ip route replace local default dev lo table "$TABLE"

if [ -f /proc/net/if_inet6 ]; then
    { ip -6 rule show | grep -Eq "fwmark 0x0*$MARK lookup $TABLE" \
        || ip -6 rule add fwmark "$MARK" lookup "$TABLE"; } \
        && ip -6 route replace local default dev lo table "$TABLE" \
        || echo "tproxy-setup: IPv6 return path not configured" >&2
fi

# 2. The divert rules that set the mark.
nft -f "$HERE/tproxy.nft"

echo "tproxy-setup: return path ready (fwmark $MARK -> table $TABLE)"
