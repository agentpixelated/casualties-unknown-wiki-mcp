# cu-wiki-mcp — Rust binary on debian-slim.
#
# The binary is dynamically linked against glibc, so the runtime image
# must be a glibc base (debian-slim), NOT distroless-static. It also
# needs the system CA roots (reqwest uses rustls-tls-native-roots).
# Build with: docker build -t ghcr.io/agentpixelated/casualties-unknown-wiki-mcp:1.0.0 .
FROM rust:1-bookworm AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/cu-wiki-mcp /usr/local/bin/cu-wiki-mcp
RUN mkdir -p /app/cache
ENV CU_CACHE_DIR=/app/cache
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/cu-wiki-mcp"]
CMD ["--http", "8080"]
