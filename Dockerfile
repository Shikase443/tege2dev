FROM docker.io/library/rust:1.95-bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends \
    clang cmake libssl-dev mold pkg-config protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src/codex-rs
ENV CARGO_PROFILE_RELEASE_DEBUG=0 RUSTFLAGS="-C link-arg=-fuse-ld=mold"
COPY codex-rs/ ./
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,id=tege2dev-release-target,target=/src/codex-rs/target \
    cargo build --locked --release -p codex-voiceagent-server \
    && cp target/release/codex-voiceagent-server /tmp/tege2dev-server

FROM docker.io/library/debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    bash bubblewrap ca-certificates curl git python3 python3-aiohttp ripgrep tini \
    bison build-essential cmake flex gperf libffi-dev libssl-dev libusb-1.0-0 \
    ninja-build python3-dev python3-pip python3-venv wget nodejs npm ffmpeg openssh-client \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 1000 agent \
    && mkdir -p /data/characters/development /data/codex /data/state /workspace \
    && chown -R agent:agent /data
COPY --from=builder /tmp/tege2dev-server /usr/local/bin/codex-agent-server
COPY scripts/entrypoint /usr/local/bin/tege2dev-entrypoint
COPY cli/tege2dev /usr/local/bin/tege2dev
COPY runtime/web/ /data/web/
RUN chmod 0755 /usr/local/bin/tege2dev /usr/local/bin/tege2dev-entrypoint
USER agent
ENV HOME=/home/agent LANG=C.UTF-8 LC_ALL=C.UTF-8 \
    CODEX_HOME=/data/codex CODEX_AGENT_DATA_DIR=/data/state \
    CODEX_AGENT_WORKSPACE=/workspace CODEX_AGENT_OUTPUT_WORKSPACE=/workspace \
    CODEX_AGENT_CHARACTER_PROMPT=/data/characters/development/AGENTS.md \
    CODEX_AGENT_MODEL_PROMPT=/data/prompts/default.md CODEX_AGENT_WEB_ROOT=/data/web \
    CODEX_AGENT_CORE_PORT=8090 PORT=8091 RUST_LOG=info
EXPOSE 8091
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/tege2dev-entrypoint"]
