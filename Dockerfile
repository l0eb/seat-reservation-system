# syntax=docker/dockerfile:1

# Builds the api service only; the burst tool runs from a checkout.
# cargo-chef caches the dependency build in its own layer, so a code
# change rebuilds just the crate (~20s) instead of every dependency.

FROM lukemathwalker/cargo-chef:latest-rust-1.93-bookworm AS chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json -p api
COPY . .
# Migrations are embedded in the binary by sqlx::migrate! at compile time.
RUN cargo build --release -p api --locked

# glibc + libgcc only, no shell; runs as uid 65532. TLS is rustls, so no
# OpenSSL is needed. Matches the bookworm builder's glibc.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /app/target/release/api /usr/local/bin/api
ENV PORT=8080 RUST_LOG=info
EXPOSE 8080
HEALTHCHECK --interval=5s --timeout=4s --start-period=40s --retries=3 \
  CMD ["/usr/local/bin/api", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/api"]
