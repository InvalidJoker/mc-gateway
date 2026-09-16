#!/bin/sh
# Verifies transparent forwarding in Docker: a client connects through the
# gateway, and the backend must see the client's own address and port — not the
# gateway's — and its reply must make it back.
set -eu
cd "$(dirname "$0")"

cleanup() { docker compose down --volumes >/dev/null 2>&1 || true; }
trap cleanup EXIT

docker compose up -d --build --wait-timeout 120 >/dev/null
# Health checks and the return-path setup need a moment.
sleep 5

output=$(docker compose exec -T client python /client.py gateway 25565)
echo "$output"

client=$(echo "$output" | sed -n 's/^CLIENT local address //p')
received=$(echo "$output" | sed -n 's/^CLIENT received: peer=//p')

if [ -n "$client" ] && [ "$client" = "$received" ]; then
    echo "PASS: backend saw the client's own address ($client)"
else
    echo "FAIL: client was ${client:-?}, backend saw ${received:-nothing}" >&2
    docker compose logs gateway echo-net >&2
    exit 1
fi
