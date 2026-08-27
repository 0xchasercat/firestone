ARG RUST_BUILDER_IMAGE=rust@sha256:a10e64dd139b7387337c7fbe8aca31b959b57b2fd4c8ae20a02cf1d6ea424dce
ARG RUST_CROSS_HOST_IMAGE=rust@sha256:4e4a7e7939c17991ab35f2b8c2e67593980f771d28f6b1254b1850f860fd0c7f
ARG CROSS_BUILDER_IMAGE=ghcr.io/cross-rs/aarch64-unknown-linux-musl@sha256:f604e399cbb2154ddeb013db99eb4f123d24f09a579c7e8d6ed631d15ffa8b12

FROM ${RUST_BUILDER_IMAGE} AS rust-toolchain

ARG RUST_VERSION
RUN test "$(rustc --version | awk '{print $2}')" = "${RUST_VERSION}"

FROM rust-toolchain AS builder-x86_64

ARG RUST_VERSION
RUN rustup target add --toolchain "${RUST_VERSION}" x86_64-unknown-linux-musl

FROM ${RUST_CROSS_HOST_IMAGE} AS rust-cross-host-toolchain

ARG RUST_VERSION
RUN test "$(rustc --version | awk '{print $2}')" = "${RUST_VERSION}"

FROM ${CROSS_BUILDER_IMAGE} AS builder-aarch64-base

COPY --from=rust-cross-host-toolchain /usr/local/cargo /usr/local/cargo
COPY --from=rust-cross-host-toolchain /usr/local/rustup /usr/local/rustup

ENV CARGO_HOME=/usr/local/cargo \
    RUSTUP_HOME=/usr/local/rustup \
    PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

FROM builder-aarch64-base AS builder-aarch64

ARG RUST_VERSION
RUN rustup target add --toolchain "${RUST_VERSION}" aarch64-unknown-linux-musl
