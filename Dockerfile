FROM rust:slim AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:trixie-slim
# The tools the generated network setup script uses.
RUN apt-get update \
    && apt-get install -y --no-install-recommends iproute2 nftables iptables \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home --shell /usr/sbin/nologin mc-gateway
COPY --from=build /src/target/release/mc-gateway /usr/local/bin/mc-gateway
COPY deploy/entrypoint.sh /usr/local/bin/entrypoint
# No `USER`: the entrypoint sets up the network as root, then drops privileges.
ENTRYPOINT ["/usr/local/bin/entrypoint"]
CMD ["--config", "/etc/mc-gateway/config.yaml"]
