# syntax=docker/dockerfile:1.10

ARG RUST_IMAGE=docker.io/library/rust:1.97.0-bookworm@sha256:7d0723df719e7f213b69dc7c8c595985c3f4b060cfbee4f7bc0e347a86fe3b6a
ARG DEBIAN_IMAGE=docker.io/library/debian:bookworm-slim@sha256:60eac759739651111db372c07be67863818726f754804b8707c90979bda511df

FROM ${RUST_IMAGE} AS builder
WORKDIR /source
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY xtask ./xtask
RUN cargo build --locked --release --package codex-start-proxy \
    --bin codex-start-sidecar --bin codex-start-init

FROM ${DEBIAN_IMAGE}
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=builder /source/target/release/codex-start-sidecar /usr/local/bin/codex-start-sidecar
COPY --from=builder /source/target/release/codex-start-init /usr/local/bin/codex-start-init
USER 65532:65532
EXPOSE 3128
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=10s --timeout=3s --start-period=3s --retries=3 \
    CMD ["/usr/local/bin/codex-start-sidecar", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/codex-start-sidecar"]
