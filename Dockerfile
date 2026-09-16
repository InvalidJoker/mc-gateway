# Build
FROM rust:slim AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p mc-gateway

# Run
FROM debian:trixie-slim

# iproute2 and nftables set up the TPROXY return path at start-up.
RUN apt-get update \
    && apt-get install -y --no-install-recommends iproute2 nftables \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home --shell /usr/sbin/nologin mc-gateway

COPY --from=build /src/target/release/mc-gateway /usr/local/bin/mc-gateway
COPY deploy/nftables/tproxy-setup.sh deploy/nftables/tproxy.nft /usr/local/lib/mc-gateway/
COPY deploy/docker/entrypoint.sh /usr/local/bin/entrypoint

EXPOSE 25565 9100

# Deliberately no `USER`: the entrypoint needs root for the network setup, then
# drops to `mc-gateway` itself, carrying CAP_NET_ADMIN across as an ambient
# capability. Run the container with `cap_add: [NET_ADMIN]`.
#
# Set MC_GATEWAY_TPROXY_SETUP=0 if the return path is configured elsewhere.
ENTRYPOINT ["/usr/local/bin/entrypoint"]
CMD ["--config", "/etc/mc-gateway/config.yaml"]
