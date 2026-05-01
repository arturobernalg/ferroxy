# syntax=docker/dockerfile:1.7
#
# Multi-stage Dockerfile for Conduit.
# - Build stage uses the musl target so the runtime stage has no glibc dependency.
# - Final stage is `gcr.io/distroless/static` — no shell, no package manager, ~2 MB.

ARG TARGETARCH
ARG RUST_VERSION=1.82

# ---------------------------------------------------------------------
# Build stage
# ---------------------------------------------------------------------
FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-slim AS builder

ARG TARGETARCH
ARG TARGETPLATFORM

# musl cross-compilation toolchains. Map Docker's TARGETARCH (amd64,
# arm64) to Rust's target triples.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        musl-tools \
        gcc-aarch64-linux-gnu \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

ENV CARGO_TERM_COLOR=always

RUN set -eux; \
    case "${TARGETARCH}" in \
        amd64) target=x86_64-unknown-linux-musl;; \
        arm64) target=aarch64-unknown-linux-musl;; \
        *) echo "unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 1;; \
    esac; \
    rustup target add "${target}"; \
    cargo build --release --target "${target}" -p conduit; \
    install -D "target/${target}/release/conduit" "/out/conduit"

# ---------------------------------------------------------------------
# Runtime stage
# ---------------------------------------------------------------------
FROM gcr.io/distroless/static:nonroot AS runtime

LABEL org.opencontainers.image.title="conduit"
LABEL org.opencontainers.image.description="A correctness-first HTTP reverse proxy for Linux."
LABEL org.opencontainers.image.licenses="Apache-2.0 OR MIT"
LABEL org.opencontainers.image.source="https://github.com/arturobernalg/conduit"

# Default config layout. Operators mount /etc/conduit with their own
# config + cert material on top of the image.
COPY --from=builder /out/conduit /usr/local/bin/conduit

EXPOSE 80 443
USER nonroot:nonroot

ENTRYPOINT ["/usr/local/bin/conduit"]
CMD ["--config", "/etc/conduit/conduit.toml"]
