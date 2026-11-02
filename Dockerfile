# syntax=docker/dockerfile:1
# state-export, built against one ever-node revision (the database format must match the
# node's). The builder runs Debian 11 natively on the build machine and cross-compiles when the
# target is the other architecture. OpenSSL and zstd are linked in, so the binary runs on
# Debian 11-13 and Ubuntu 22.04/24.04, on x86_64 and arm64.
#   docker buildx build --platform linux/amd64,linux/arm64 -t state-export:<ever-node version> --load .
#   docker build --target binary --output . .                                            (only the binary)
#   docker build --target test .                                                         (unit tests)
#   --build-arg EVER_NODE_REV=<commit>                                                   (another ever-node revision)

FROM --platform=$BUILDPLATFORM debian:bullseye AS builder
ARG JOBS=6
ARG RUST_VERSION=1.99.0
ARG EVER_NODE_REV=e4068621a96d78969dd6df3087504d25b06fca6e
ARG BUILDARCH
ARG TARGETARCH
# Debian 11 is past its end of life and its security repository on deb.debian.org is incomplete.
# Install from the snapshot the image was made from (the image lists it, commented out).
RUN sed -i -e 's|^# deb http://snapshot|deb http://snapshot|' -e '/^deb http:\/\/deb.debian.org/d' /etc/apt/sources.list \
 && case ${TARGETARCH} in \
        amd64) cross="gcc-x86-64-linux-gnu g++-x86-64-linux-gnu libc6-dev-amd64-cross" ;; \
        arm64) cross="gcc-aarch64-linux-gnu g++-aarch64-linux-gnu libc6-dev-arm64-cross" ;; \
        *) echo "unsupported architecture ${TARGETARCH}"; exit 1 ;; \
    esac \
 && if [ "${TARGETARCH}" = "${BUILDARCH}" ]; then cross="gcc g++"; else dpkg --add-architecture ${TARGETARCH}; fi \
 && apt-get -o Acquire::Check-Valid-Until=false update \
 && apt-get install -y --no-install-recommends \
    ca-certificates curl git pkg-config make cmake clang libclang-dev perl libzstd-dev:${TARGETARCH} ${cross} \
 && rm -rf /var/lib/apt/lists/*
# Compiler, linker and library settings for the target, kept in /build.env.
# ever-adnl asks for zstd through pkg-config. The pkg-config crate links a library statically only
# when it lies outside the system folders, so Debian's static libzstd.a is exposed from /opt/zstd.
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain ${RUST_VERSION} --profile minimal \
 && case ${TARGETARCH} in amd64) arch=x86_64 ;; arm64) arch=aarch64 ;; esac \
 && gnu=${arch}-linux-gnu && rust=${arch}-unknown-linux-gnu && var=$(echo ${rust} | tr '-' '_') \
 && /root/.cargo/bin/rustup target add ${rust} \
 && mkdir -p /opt/zstd/lib/pkgconfig \
 && cp /usr/lib/${gnu}/libzstd.a /opt/zstd/lib/ \
 && sed "s|^libdir=.*|libdir=/opt/zstd/lib|" /usr/lib/${gnu}/pkgconfig/libzstd.pc > /opt/zstd/lib/pkgconfig/libzstd.pc \
 && { echo "export CARGO_BUILD_TARGET=${rust}"; \
      echo "export CARGO_TARGET_$(echo ${var} | tr a-z A-Z)_LINKER=${gnu}-gcc"; \
      echo "export CC_${var}=${gnu}-gcc CXX_${var}=${gnu}-g++ AR_${var}=${gnu}-ar"; \
      echo "export BINDGEN_EXTRA_CLANG_ARGS='--target=${gnu} -I/usr/${gnu}/include'"; \
      echo "export PKG_CONFIG_ALLOW_CROSS=1 PKG_CONFIG_PATH=/opt/zstd/lib/pkgconfig PKG_CONFIG_ALL_STATIC=1"; \
    } > /build.env
ENV PATH="/root/.cargo/bin:${PATH}" CARGO_TARGET_DIR=/src/target/bullseye
WORKDIR /src
COPY Cargo.toml Cargo.lock build.rs set-ever-node-rev.sh ./
COPY src ./src
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/src/target \
    . /build.env \
 && if ! grep -q "rev = \"${EVER_NODE_REV}\"" Cargo.toml; then sh set-ever-node-rev.sh "${EVER_NODE_REV}"; fi \
 # COPY keeps file times, which can be older than the cached build: force our crate to recompile.
 && touch build.rs src/main.rs \
 && cargo build --release --jobs ${JOBS} \
 && cp ${CARGO_TARGET_DIR}/${CARGO_BUILD_TARGET}/release/state-export /state-export

# Unit tests, on the build machine's own architecture: docker build --target test .
FROM builder AS test
ARG JOBS=6
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/src/target \
    . /build.env && touch build.rs src/main.rs && cargo test --release --jobs ${JOBS}

# Only the binary: docker build --target binary --output . .
FROM scratch AS binary
COPY --from=builder /state-export /

FROM debian:bookworm-slim
COPY --from=builder /state-export /usr/local/bin/state-export
ENTRYPOINT ["/usr/local/bin/state-export"]
