FROM rust:1.98-bookworm AS builder

# This Rust utility gives the diagnostic agent a safe, well-supported way to inspect Windows
# minidumps included in ShieldBattery reports. Build it in a source-independent layer so normal
# Adjutant changes retain the cache.
RUN cargo install minidump-stackwalk --version 0.27.0 --locked --root /opt/minidump

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY migrations ./migrations
COPY src ./src
RUN cargo build --locked --release

FROM node:24-bookworm-slim AS runtime

ARG CODEX_VERSION=0.150.1
ENV CODEX_HOME=/var/lib/adjutant/codex \
    HOME=/var/lib/adjutant \
    NPM_CONFIG_UPDATE_NOTIFIER=false

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates file git jq ripgrep util-linux \
    && npm install --global "@openai/codex@${CODEX_VERSION}" \
    && npm cache clean --force \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 adjutant \
    && useradd --uid 10001 --gid adjutant --home-dir /var/lib/adjutant --shell /bin/sh adjutant \
    && install -d -o adjutant -g adjutant /var/lib/adjutant/codex /var/lib/adjutant/data \
    && install -d -o adjutant -g adjutant /workspace/shieldbattery

COPY --from=builder /build/target/release/adjutant /usr/local/bin/adjutant
COPY --from=builder /opt/minidump/bin/minidump-stackwalk /usr/local/bin/minidump-stackwalk

WORKDIR /workspace/shieldbattery
EXPOSE 8080

# Compose's init shim remains root so its inherited environment is unreadable to the diagnostic
# child. setpriv immediately drops the actual service (and every Codex descendant) to UID/GID 10001.
# The container receives only SETUID/SETGID during this one-way handoff; no-new-privileges prevents
# the non-root process from regaining them.
ENTRYPOINT ["/usr/bin/setpriv", "--reuid=10001", "--regid=10001", "--init-groups", "--"]
CMD ["/usr/local/bin/adjutant"]
