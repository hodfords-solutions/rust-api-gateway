# rust-gateway-server, release build. Build context: the repository root.
# rust-toolchain.toml is left out on purpose: it names "stable", which would make rustup
# download the newest toolchain instead of using the pinned image.
FROM rust:1.100-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY conformance/harness conformance/harness
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p rust-gateway-server \
    && mkdir -p /out/bin /out/share \
    && cp target/release/rust-gateway-server /out/bin/ \
    && rustc --version > /out/share/rustc-version

FROM debian:bookworm-slim
# The document fetcher's HTTP client (reqwest with rustls) will not build without CA
# certificates, even for plain-HTTP upstreams.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /out/bin/ /usr/local/bin/
COPY --from=build /out/share/ /usr/local/share/rust-gateway/
ENV GATEWAY_MIDDLEWARE=bench RUST_LOG=warn NO_COLOR=1
CMD ["rust-gateway-server"]
