# The benchmark's mock Upstream Service. Build context: the repository root.
FROM rust:1.100-slim-bookworm AS build
WORKDIR /src/benches
COPY benches/fixtures fixtures
COPY benches/upstream upstream
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/benches/upstream/target \
    cd upstream && cargo build --release --locked \
    && cp target/release/bench-upstream /bench-upstream

FROM debian:bookworm-slim
COPY --from=build /bench-upstream /usr/local/bin/
CMD ["bench-upstream"]
