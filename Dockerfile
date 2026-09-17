# syntax=docker/dockerfile:1

# Compiles on the build machine's own architecture and cross-links for the
# target, instead of emulating the target: a Rust release build under QEMU takes
# many times longer.
FROM --platform=$BUILDPLATFORM rust:1-slim-trixie AS build
ARG BUILDARCH
ARG TARGETARCH
WORKDIR /src

RUN set -eu; \
    case "$TARGETARCH" in \
        amd64) target=x86_64-unknown-linux-gnu;  cross=x86-64;  prefix=x86_64-linux-gnu ;; \
        arm64) target=aarch64-unknown-linux-gnu; cross=aarch64; prefix=aarch64-linux-gnu ;; \
        *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    if [ "$TARGETARCH" = "$BUILDARCH" ]; then \
        packages=gcc; linker=gcc; \
    else \
        packages="gcc-$cross-linux-gnu libc6-dev-$TARGETARCH-cross"; linker="$prefix-gcc"; \
    fi; \
    apt-get update; \
    apt-get install -y --no-install-recommends $packages; \
    rm -rf /var/lib/apt/lists/*; \
    rustup target add "$target"; \
    echo "$target" > /target; \
    echo "$linker" > /linker

COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN set -eu; \
    target=$(cat /target); \
    env "CARGO_TARGET_$(echo "$target" | tr 'a-z-' 'A-Z_')_LINKER=$(cat /linker)" \
        cargo build --release --locked --target "$target"; \
    cp "target/$target/release/mc-gateway" /mc-gateway

FROM debian:trixie-slim
# The tools the generated network setup script uses.
RUN apt-get update \
    && apt-get install -y --no-install-recommends iproute2 nftables iptables \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home --shell /usr/sbin/nologin mc-gateway
COPY --from=build /mc-gateway /usr/local/bin/mc-gateway
COPY deploy/entrypoint.sh /usr/local/bin/entrypoint
LABEL org.opencontainers.image.source="https://github.com/InvalidJoker/mc-gateway"
# No `USER`: the entrypoint sets up the network as root, then drops privileges.
ENTRYPOINT ["/usr/local/bin/entrypoint"]
CMD ["--config", "/etc/mc-gateway/config.yaml"]
