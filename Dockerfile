# syntax=docker/dockerfile:1

# ---- Build stage: full Rust toolchain ----
FROM rust:1-trixie AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

# Work out which dependencies the workspace needs.
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Build dependencies first. This layer only rebuilds when Cargo.toml or
# Cargo.lock change, so code-only changes rebuild in a fraction of the time.
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

COPY . .
# No database at build time: sqlx's query! macros use the saved .sqlx data.
ENV SQLX_OFFLINE=true
RUN cargo build --release -p labwatch-server

# ---- Runtime stage: just the binary ----
FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/labwatch-server /usr/local/bin/labwatch

ENTRYPOINT ["labwatch"]
CMD ["serve", "--config", "/config/config.toml"]
