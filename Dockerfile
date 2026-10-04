# syntax=docker/dockerfile:1

# Builds the api service only; the burst tool runs from a checkout.
#   docker build .                              image for this machine
#   docker buildx build --platform linux/arm64  image for Graviton (AWS)
# The compiler always runs natively and cross-compiles for the target with
# cargo-zigbuild (zig as the C toolchain and linker), so an arm64 image
# builds at native speed rather than under emulation. cargo-chef caches the
# dependency build in its own layer, so a code change rebuilds only the
# crate.

FROM --platform=$BUILDPLATFORM lukemathwalker/cargo-chef:latest-rust-1.93-bookworm AS chef
ARG BUILDARCH
ARG ZIG_VERSION=0.13.0
RUN case "$BUILDARCH" in amd64) zarch=x86_64 ;; arm64) zarch=aarch64 ;; *) exit 1 ;; esac \
 && curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/zig-linux-${zarch}-${ZIG_VERSION}.tar.xz" \
    | tar -xJ -C /opt \
 && ln -s "/opt/zig-linux-${zarch}-${ZIG_VERSION}/zig" /usr/local/bin/zig \
 && cargo install cargo-zigbuild --locked \
 && rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
ARG TARGETARCH
RUN case "$TARGETARCH" in amd64) echo x86_64-unknown-linux-gnu ;; arm64) echo aarch64-unknown-linux-gnu ;; *) exit 1 ;; esac > /target
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --zigbuild --target "$(cat /target)" --recipe-path recipe.json -p api
COPY . .
# Migrations are embedded in the binary by sqlx::migrate! at compile time.
RUN cargo zigbuild --release -p api --locked --target "$(cat /target)" \
 && cp "target/$(cat /target)/release/api" /api

# glibc + libgcc only, no shell; runs as uid 65532. TLS is rustls, so no
# OpenSSL is needed. zigbuild targets an older glibc than debian12's 2.36,
# so the binary runs here.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /api /usr/local/bin/api
ENV PORT=8080 RUST_LOG=info
EXPOSE 8080
HEALTHCHECK --interval=5s --timeout=4s --start-period=40s --retries=3 \
  CMD ["/usr/local/bin/api", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/api"]
