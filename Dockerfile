# Build
FROM rust:slim AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p mc-gateway

# Run
FROM debian:trixie-slim
RUN useradd --system --no-create-home --shell /usr/sbin/nologin mc-gateway
COPY --from=build /src/target/release/mc-gateway /usr/local/bin/mc-gateway
USER mc-gateway
EXPOSE 25565 9100
ENTRYPOINT ["/usr/local/bin/mc-gateway"]
CMD ["--config", "/etc/mc-gateway/config.yaml"]
