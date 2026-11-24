# syntax=docker/dockerfile:1

# ---- build ---------------------------------------------------------------
FROM rust:1.90-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p elkarbackup \
    && install -Dm755 target/release/elkarbackup /out/elkarbackup

# ---- runtime -------------------------------------------------------------
FROM debian:bookworm-slim
# rsnapshot/rsync/ssh run the backups; tini reaps their children;
# tzdata lets TZ drive the policy schedules; curl serves the health check.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        rsnapshot rsync openssh-client ca-certificates tzdata tini curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /out/elkarbackup /usr/local/bin/elkarbackup
COPY docker/elkarbackup.toml /etc/elkarbackup/elkarbackup.toml
COPY docker/entrypoint.sh /usr/local/bin/docker-entrypoint.sh
COPY LICENSE /usr/share/doc/elkarbackup/LICENSE

ENV ELKARBACKUP_CONFIG=/etc/elkarbackup/elkarbackup.toml \
    ELKARBACKUP_LISTEN=0.0.0.0:8080 \
    TZ=UTC
VOLUME ["/var/lib/elkarbackup"]
EXPOSE 8080
# Runs as root on purpose: snapshots must keep the original file owners.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s \
    CMD curl -fsS http://127.0.0.1:8080/healthz || exit 1
ENTRYPOINT ["tini", "--", "docker-entrypoint.sh"]
CMD ["serve"]
