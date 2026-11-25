# Container image for @bitefoundry/outlook (stdio MCP server, CLI, and TUI).
#
# Installs the verified Linux release binary (GLIBC <= 2.28) directly from
# GitHub Releases instead of compiling Rust from source, so builds complete in
# seconds and match the published release artifact.
FROM debian:bookworm-slim

ARG OUTLOOK_VERSION=0.3.5
ARG TARGETARCH

LABEL org.opencontainers.image.title="outlook" \
      org.opencontainers.image.description="Single Rust binary pairing a stdio MCP server, CLI, local HTTP API, and Ratatui TUI for Microsoft Outlook mail, calendar, and tasks over an embedded SQLite WAL + FTS5 cache." \
      org.opencontainers.image.source="https://github.com/BiteFoundry/outlook" \
      org.opencontainers.image.documentation="https://github.com/BiteFoundry/outlook#readme" \
      org.opencontainers.image.version="${OUTLOOK_VERSION}" \
      org.opencontainers.image.licenses="MIT" \
      io.modelcontextprotocol.server.name="io.github.BiteFoundry/outlook"

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*

RUN set -eu; \
    arch="${TARGETARCH:-$(dpkg --print-architecture)}"; \
    case "$arch" in \
      amd64) target="x86_64-unknown-linux-gnu" ;; \
      arm64) target="aarch64-unknown-linux-gnu" ;; \
      *) echo "Unsupported architecture: $arch" >&2; exit 1 ;; \
    esac; \
    base="https://github.com/BiteFoundry/outlook/releases/download/v${OUTLOOK_VERSION}"; \
    archive="outlook-${OUTLOOK_VERSION}-${target}.tar.gz"; \
    cd /tmp; \
    curl -fsSLO "${base}/${archive}"; \
    curl -fsSLO "${base}/SHA256SUMS.txt"; \
    grep -F "  ${archive}" SHA256SUMS.txt > expected.sha256; \
    test "$(wc -l < expected.sha256)" -eq 1; \
    sha256sum -c expected.sha256; \
    tar -xzf "${archive}" outlook; \
    install -m 0755 outlook /usr/local/bin/outlook; \
    rm -f "/tmp/${archive}" /tmp/SHA256SUMS.txt /tmp/expected.sha256 /tmp/outlook; \
    outlook --version

ENV OUTLOOK_CONFIG_DIR=/data/config \
    OUTLOOK_DATA_DIR=/data/state

RUN mkdir -p /data/config /data/state \
 && chmod 0777 /data /data/config /data/state

WORKDIR /data

ENTRYPOINT ["outlook"]
CMD ["mcp"]
