#!/bin/sh
# Container entrypoint.
#
# Starts as root only long enough to set up the TPROXY return path in the
# container's own network namespace, then drops to the unprivileged
# `mc-gateway` user, keeping exactly one capability: CAP_NET_ADMIN.
#
# `USER` in the Dockerfile cannot do this. A non-root user gets no effective
# capabilities from `cap_add`, so the gateway would fail its start-up check
# even with NET_ADMIN granted. Ambient capabilities are what carry it across.
set -eu

CAP_NET_ADMIN=12

bounding=$(sed -n 's/^CapBnd:[[:space:]]*//p' /proc/self/status)
if [ $(( (0x$bounding >> CAP_NET_ADMIN) & 1 )) -ne 1 ]; then
    cat >&2 <<'MSG'
mc-gateway: this container was started without CAP_NET_ADMIN.

The gateway connects to backends transparently (TPROXY), which needs it.
Containers are Linux on every host, Docker Desktop on macOS and Windows included.

  docker run:      --cap-add NET_ADMIN
  docker compose:  cap_add: [NET_ADMIN]
MSG
    exit 1
fi

if [ "${MC_GATEWAY_TPROXY_SETUP:-1}" = "1" ]; then
    /usr/local/lib/mc-gateway/tproxy-setup.sh
fi

exec setpriv \
    --reuid=mc-gateway --regid=mc-gateway --init-groups \
    --inh-caps=-all,+net_admin \
    --ambient-caps=-all,+net_admin \
    --bounding-set=-all,+net_admin \
    --no-new-privs \
    /usr/local/bin/mc-gateway "$@"
