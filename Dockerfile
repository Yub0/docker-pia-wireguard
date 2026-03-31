FROM rust:alpine as builder
WORKDIR /app

COPY . .

RUN apk add --no-cache openssl-dev alpine-sdk

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    RUSTFLAGS=-Ctarget-feature=-crt-static cargo build --release; mv ./target/release/pia-wireguard ./pia-wireguard


FROM alpine
WORKDIR /app

RUN apk add --no-cache openssl libgcc ipcalc iptables iproute2 wireguard-tools

# Create a minimal resolvconf script that works without init system
RUN printf '#!/bin/sh\nexit 0\n' > /usr/bin/resolvconf && \
    chmod +x /usr/bin/resolvconf

COPY --from=builder /app/pia-wireguard .

ENTRYPOINT ["/app/pia-wireguard"]