# syntax=docker/dockerfile:1.10

ARG NODE_IMAGE=docker.io/library/node:24.14.1-bookworm-slim@sha256:b506e7321f176aae77317f99d67a24b272c1f09f1d10f1761f2773447d8da26c
ARG RUST_IMAGE=docker.io/library/rust:1.97.0-bookworm@sha256:7d0723df719e7f213b69dc7c8c595985c3f4b060cfbee4f7bc0e347a86fe3b6a

FROM ${RUST_IMAGE} AS helper-builder
WORKDIR /source
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY xtask ./xtask
RUN cargo build --locked --release --package codex-start-proxy --bins

FROM ${NODE_IMAGE} AS node-source

FROM node-source AS generic-base
ARG CODEX_VERSION=0.142.5
ARG CODEX_SHA512=5901290fb97793af1e2003f46aadbc11d475f0434101ff03ca9af3161ccdc02390252bf89c7ce9c13f05977d0825a729ae4a366429940598ccdaef78d65db22f
LABEL io.codex-start.codex.version="${CODEX_VERSION}" \
      io.codex-start.codex.wrapper-sha512="${CODEX_SHA512}"
ENV DEBIAN_FRONTEND=noninteractive \
    HOME=/home/codex \
    GH_CONFIG_DIR=/home/codex/.config/gh \
    NPM_CONFIG_CACHE=/home/codex/.cache/npm \
    NPM_CONFIG_PREFIX=/home/codex/.local \
    PATH=/home/codex/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash bc bind9-dnsutils bsdmainutils build-essential ca-certificates coreutils curl \
        dnsutils fd-find file gh git gnupg htop iproute2 iputils-ping jq less lsof man-db \
        nano ncdu netcat-openbsd openssh-client passwd pkg-config procps psmisc python3 python3-dev \
        python3-pip python3-venv ripgrep rsync socat sudo tmux traceroute tree unzip vim-tiny wget \
        xz-utils zip zstd libbz2-dev libffi-dev liblzma-dev libreadline-dev libsqlite3-dev \
        libssl-dev tk-dev zlib1g-dev \
    && rm -rf /var/lib/apt/lists/* \
    && /usr/sbin/usermod --login codex --home /home/codex --move-home --shell /bin/bash node \
    && /usr/sbin/groupmod --new-name codex node \
    && install -d -o codex -g codex \
        /workspaces /home/codex/.agents /home/codex/.cache/npm /home/codex/.codex \
        /home/codex/.config/gh /home/codex/.gnupg /home/codex/.local/bin /home/codex/.ssh \
    && chmod 0700 /home/codex/.gnupg /home/codex/.ssh \
    && printf 'codex ALL=(ALL) NOPASSWD:ALL\n' >/etc/sudoers.d/codex \
    && chmod 0440 /etc/sudoers.d/codex
COPY images/environment/codex-package.json /opt/codex/package.json
COPY images/environment/codex-package-lock.json /opt/codex/package-lock.json
RUN cd /opt/codex \
    && npm ci --ignore-scripts --omit=dev \
    && ln -s /opt/codex/node_modules/.bin/codex /usr/local/bin/codex \
    && npm cache clean --force
COPY --from=helper-builder /source/target/release/codex-start-init /usr/local/bin/codex-start-init
COPY --from=helper-builder /source/target/release/codex-start-host-ssh /usr/local/bin/codex-start-host-ssh
COPY --from=helper-builder /source/target/release/codex-start-ssh /usr/local/bin/ssh
COPY --from=helper-builder /source/target/release/codex-start-sidecar /usr/local/bin/codex-start-sidecar
WORKDIR /workspaces
ENTRYPOINT ["/usr/local/bin/codex-start-init", "run", "--"]
CMD ["codex"]

FROM generic-base AS generic

FROM generic-base AS web
EXPOSE 4173 5173

FROM generic-base AS uv
ARG TARGETARCH
ARG UV_VERSION=0.11.28
ARG JUST_VERSION=1.55.1
ARG UV_SHA256_AMD64=e490a6464492183c5d4534a5527fb4440f7f2bb2f228162ad7e4afe076dc0224
ARG UV_SHA256_ARM64=03e9fe0a81b0718d0bc84625de3885df6cc3f89a8b6af6121d6b9f6113fb6533
ARG JUST_SHA256_AMD64=b0ef600f0df20d5ae91ae931627c499fc52b477ffe5f5ea7b7b3ec616b16c778
ARG JUST_SHA256_ARM64=b0ee814c9656427408e339893541e30d9027828686839499b2a2a34dd61ad173
ENV UV_CACHE_DIR=/home/codex/.cache/uv \
    UV_PYTHON_CACHE_DIR=/home/codex/.cache/uv/python \
    UV_PROJECT_ENVIRONMENT=/home/codex/.venv \
    UV_TOOL_BIN_DIR=/home/codex/.local/bin \
    UV_TOOL_DIR=/home/codex/.local/share/uv/tools \
    UV_LINK_MODE=copy \
    PATH=/home/codex/.venv/bin:/home/codex/.local/bin:/usr/local/bin:/usr/bin:/bin
RUN set -eu; \
    case "${TARGETARCH}" in \
      amd64) uv_target=x86_64-unknown-linux-gnu; uv_sha256="${UV_SHA256_AMD64}"; just_target=x86_64-unknown-linux-musl; just_sha256="${JUST_SHA256_AMD64}" ;; \
      arm64) uv_target=aarch64-unknown-linux-gnu; uv_sha256="${UV_SHA256_ARM64}"; just_target=aarch64-unknown-linux-musl; just_sha256="${JUST_SHA256_ARM64}" ;; \
      *) echo "unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 64 ;; \
    esac; \
    archive="uv-${uv_target}.tar.gz"; \
    curl --fail --location --proto '=https' --tlsv1.2 \
      --output "/tmp/${archive}" \
      "https://github.com/astral-sh/uv/releases/download/${UV_VERSION}/${archive}"; \
    echo "${uv_sha256}  /tmp/${archive}" | sha256sum --check --strict; \
    tar --extract --gzip --file "/tmp/${archive}" --directory /usr/local/bin \
      --strip-components=1 "uv-${uv_target}/uv" "uv-${uv_target}/uvx"; \
    rm "/tmp/${archive}"; \
    uv --version; \
    just_archive="just-${JUST_VERSION}-${just_target}.tar.gz"; \
    curl --fail --location --proto '=https' --tlsv1.2 \
      --output "/tmp/${just_archive}" \
      "https://github.com/casey/just/releases/download/${JUST_VERSION}/${just_archive}"; \
    echo "${just_sha256}  /tmp/${just_archive}" | sha256sum --check --strict; \
    tar --extract --gzip --file "/tmp/${just_archive}" --directory /usr/local/bin just; \
    rm "/tmp/${just_archive}"; \
    just --version; \
    install -d -o codex -g codex \
      /home/codex/.cache/uv /home/codex/.venv /home/codex/.local/share/uv/tools

FROM ${RUST_IMAGE} AS rust
ARG CODEX_VERSION=0.142.5
ARG CODEX_SHA512=5901290fb97793af1e2003f46aadbc11d475f0434101ff03ca9af3161ccdc02390252bf89c7ce9c13f05977d0825a729ae4a366429940598ccdaef78d65db22f
LABEL io.codex-start.codex.version="${CODEX_VERSION}" \
      io.codex-start.codex.wrapper-sha512="${CODEX_SHA512}"
ENV DEBIAN_FRONTEND=noninteractive \
    HOME=/home/codex \
    GH_CONFIG_DIR=/home/codex/.config/gh \
    CARGO_HOME=/home/codex/.cargo \
    CARGO_TARGET_DIR=/home/codex/.cache/cargo-target \
    NPM_CONFIG_CACHE=/home/codex/.cache/npm \
    NPM_CONFIG_PREFIX=/home/codex/.local \
    PATH=/home/codex/.cargo/bin:/home/codex/.local/bin:/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
COPY --from=node-source /usr/local/bin/node /usr/local/bin/node
COPY --from=node-source /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -s /usr/local/lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -s /usr/local/lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        bash bc bind9-dnsutils bsdmainutils ca-certificates coreutils curl dnsutils fd-find file \
        gh git gnupg htop iproute2 iputils-ping jq less lsof man-db nano ncdu netcat-openbsd \
        openssh-client passwd procps psmisc ripgrep rsync socat sudo tmux traceroute tree unzip vim-tiny wget \
        xz-utils zip zstd autoconf automake binutils binutils-dev build-essential ccache clang \
        clang-format clang-tidy clang-tools cmake dwarves elfutils gdb libclang-dev libssl-dev \
        libtool libzstd-dev lld lldb llvm llvm-dev llvm-runtime mold ninja-build pax-utils \
        pkg-config protobuf-compiler strace valgrind zlib1g-dev \
    && rm -rf /var/lib/apt/lists/* \
    && rustup set profile default \
    && rustup component add clippy rustfmt rust-src rust-analyzer llvm-tools-preview \
    && /usr/sbin/useradd --create-home --uid 1000 --user-group --shell /bin/bash codex \
    && install -d -o codex -g codex \
        /workspaces /home/codex/.agents /home/codex/.cache/cargo-target /home/codex/.cache/npm \
        /home/codex/.cargo /home/codex/.codex /home/codex/.config/gh /home/codex/.gnupg \
        /home/codex/.local/bin /home/codex/.ssh \
    && chmod 0700 /home/codex/.gnupg /home/codex/.ssh \
    && printf 'codex ALL=(ALL) NOPASSWD:ALL\n' >/etc/sudoers.d/codex \
    && chmod 0440 /etc/sudoers.d/codex
COPY images/environment/codex-package.json /opt/codex/package.json
COPY images/environment/codex-package-lock.json /opt/codex/package-lock.json
RUN cd /opt/codex \
    && npm ci --ignore-scripts --omit=dev \
    && ln -s /opt/codex/node_modules/.bin/codex /usr/local/bin/codex \
    && npm cache clean --force
COPY --from=helper-builder /source/target/release/codex-start-init /usr/local/bin/codex-start-init
COPY --from=helper-builder /source/target/release/codex-start-host-ssh /usr/local/bin/codex-start-host-ssh
COPY --from=helper-builder /source/target/release/codex-start-ssh /usr/local/bin/ssh
COPY --from=helper-builder /source/target/release/codex-start-sidecar /usr/local/bin/codex-start-sidecar
WORKDIR /workspaces
ENTRYPOINT ["/usr/local/bin/codex-start-init", "run", "--"]
CMD ["codex"]
