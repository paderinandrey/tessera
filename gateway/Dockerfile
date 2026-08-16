# syntax=docker/dockerfile:1

# Pinned rather than `stable`: the repository pins no toolchain, so a build
# today and a build in six months are otherwise different builds.
FROM rust:1.97-slim-trixie AS build
# openssl-sys links the system OpenSSL (see the runtime stage below for why
# it's OpenSSL and not rustls) and needs its headers and pkg-config to find
# them; the slim base has neither.
RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config libssl-dev \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
 && cp target/release/tessera-gateway /usr/local/bin/tessera-gateway

FROM debian:trixie-slim
# `reqwest` is built with default features, so the binary links OpenSSL.
# Switching to rustls would shrink this image and would also change which
# root store validates a provider's certificate — a security decision, and
# not one packaging gets to make. `curl` serves the healthcheck and the
# person debugging this container at 3am.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl libssl3 \
 && rm -rf /var/lib/apt/lists/*
# The journal and its salt live here and must share one persistent volume: a
# journal with lines and no salt beside it refuses to start. Creating the
# directory owned by the service user is what makes Docker initialise an
# empty named volume with that ownership — otherwise an unprivileged process
# meets a root-owned mount on first run.
RUN useradd --system --uid 10001 --create-home tessera \
 && mkdir -p /var/lib/tessera \
 && chown tessera:tessera /var/lib/tessera
COPY --from=build /usr/local/bin/tessera-gateway /usr/local/bin/tessera-gateway
USER tessera
EXPOSE 8080
ENTRYPOINT ["tessera-gateway"]
CMD ["/etc/tessera/tessera.toml"]
