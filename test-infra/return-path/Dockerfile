# Runs deploy/nftables/backend-return-path.sh inside a backend container's
# network namespace (compose: `network_mode: service:<backend>`).
FROM alpine:3
RUN apk add --no-cache nftables iproute2
COPY deploy/nftables/backend-return-path.sh /usr/local/bin/backend-return-path
ENTRYPOINT ["/bin/sh", "-c", "backend-return-path && exec sleep infinity"]
