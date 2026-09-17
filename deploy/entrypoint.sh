#!/bin/sh
# Container entrypoint: installs the firewall and routing rules as root, then
# runs the gateway as the unprivileged `mc-gateway` user with only
# CAP_NET_ADMIN, which it needs for transparent sockets.
#
# A `USER` line in the Dockerfile cannot do this: a non-root user gets no
# effective capabilities from `cap_add`. Ambient capabilities carry it across.
set -eu

CAP_NET_ADMIN=12
bounding=$(sed -n 's/^CapBnd:[[:space:]]*//p' /proc/self/status)
if [ $(( (0x$bounding >> CAP_NET_ADMIN) & 1 )) -ne 1 ]; then
    echo "mc-gateway: start the container with NET_ADMIN (compose: cap_add: [NET_ADMIN])" >&2
    exit 1
fi

# Captured first, so a config error stops here instead of running an empty script.
setup=$(/usr/local/bin/mc-gateway "$@" --print-network-setup)
printf '%s\n' "$setup" | sh

exec setpriv \
    --reuid=mc-gateway --regid=mc-gateway --init-groups \
    --inh-caps=-all,+net_admin \
    --ambient-caps=-all,+net_admin \
    --bounding-set=-all,+net_admin \
    --no-new-privs \
    /usr/local/bin/mc-gateway "$@"
