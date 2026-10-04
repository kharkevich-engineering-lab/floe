# floe image — the GitHub Enterprise Server facade for local development and E2Es.
#
#   podman build -t floe -f Containerfile .
#   podman run --rm -p 127.0.0.1:8097:8097 \
#       -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY \
#       -e FLOE__STORE__BUCKET=your-bucket \
#       -v ./floe.toml:/etc/floe/floe.toml:ro \
#       -v floe-cache:/var/lib/floe \
#       floe
#
# The image carries git (upload-pack, repack, bundle, index-pack run as subprocesses),
# git-lfs, CA certificates and tini. Config comes from /etc/floe/floe.toml or
# FLOE__SECTION__KEY environment overrides; the local cache (materialized repositories,
# a self-signed TLS cert) lives under /var/lib/floe and can be wiped at any time — the
# bucket is the only durable state. flake.nix provides separate standalone floe packaging.

# ---- 1. web UI (embedded into the binary at compile time) ---------------------------
FROM docker.io/library/node:24-bookworm-slim AS web
RUN corepack enable && corepack prepare pnpm@10 --activate
WORKDIR /src/web
COPY web/package.json web/pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile
COPY web/ ./
RUN pnpm run build && test -f dist/index.html && test -f dist/repos.js

# ---- 2. rust build ------------------------------------------------------------------------
FROM docker.io/library/rust:1.97-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler libprotobuf-dev pkg-config cmake perl python3 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY --from=web /src/web/dist ./web/dist
ARG FLOE_BUILD_SHA=dev
ENV FLOE_BUILD_SHA=${FLOE_BUILD_SHA}
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p floe-cli \
    && install -D target/release/floe /out/bin/floe \
    && install -D target/release/floe-server /out/bin/floe-server

# ---- 3. runtime -----------------------------------------------------------------------------
# trixie ships git 2.47+: floe wants >= 2.47 on the server (pack.writeReverseIndex, bundle-uri,
# `index-pack --rev-index`); clients need >= 2.46.
FROM docker.io/library/debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends git git-lfs ca-certificates tini curl \
    && rm -rf /var/lib/apt/lists/* \
    && git --version
RUN useradd --uid 1000 --create-home --shell /bin/sh floe \
    && mkdir -p /etc/floe /var/lib/floe && chown floe:floe /var/lib/floe
COPY --from=build /out/bin/floe /out/bin/floe-server /usr/local/bin/
COPY deploy/floe.toml /etc/floe/floe.toml
ENV FLOE_CONFIG=/etc/floe/floe.toml
USER floe
WORKDIR /home/floe
EXPOSE 8097
VOLUME ["/var/lib/floe"]
HEALTHCHECK --interval=30s --timeout=5s CMD curl -fsS http://127.0.0.1:8097/readyz || exit 1
ENTRYPOINT ["tini", "--", "floe-server"]
