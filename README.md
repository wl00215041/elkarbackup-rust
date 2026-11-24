# ElkarBackup (Rust)

A centralised backup system built on rsnapshot and rsync. It is a Rust rewrite of the PHP/Symfony [ElkarBackup](https://github.com/elkarbackup/elkarbackup), backed by PostgreSQL.

## Features

- Manage clients, jobs, policies (hourly / daily / weekly / monthly / yearly retention and schedules), scripts, backup locations and users
- Scheduler: queues jobs automatically according to their policies; runs client pre/post scripts → job pre scripts → rsnapshot → job post scripts
  - Concurrency limits globally, per client and per backup location
  - Abort running jobs (SIGTERM → SIGKILL)
  - Trigger a job anonymously with a token: `curl -X POST 'https://host/client/1/job/2/run?token=…'`
- Web interface: browse backups, download files or directories (streamed as `.tar.gz`), restore to any client
- JSON REST API (`/api/*`, HTTP Basic authentication)
- Activity log and email notifications (filtered by each job's minimum notification level)
- Roles: `admin` manages everything; `user` only sees and operates the clients they own

## Architecture

```
crates/
  elkar-core     Domain model and pure business rules (schedule calculation, validation, rsnapshot config generation); no I/O
  elkar-storage  PostgreSQL repositories and migrations (sqlx)
  elkar-engine   Execution layer: ProcessRunner, scripts, backups, scheduler state machine, notifications, restore
  elkar-web      axum: HTML interface (askama), JSON API, authentication and authorization (access layer)
  elkarbackup    Binary: serve / tick / migrate / create-admin / generate-keypair
```

Dependencies point one way: `elkarbackup → web → engine → storage → core`.

Design highlights:

- **Single daemon**: `serve` runs both the web interface and the scheduler; `tick` is a one-shot mode (suitable for cron). Both share the same `Scheduler`.
  A PostgreSQL advisory lock guarantees that only one scheduler runs at a time.
- **Scheduler state machine**: queue items go `queued → waiting_client → pre_scripts → running → post_scripts`;
  clients go `idle → pre_scripts → ready → post_scripts → idle`. Each job runs as a tokio task and is aborted through a `CancellationToken`.
- **External process abstraction**: every external command goes through the `ProcessRunner` trait, with arguments passed directly rather than through a shell; tests use a fake runner.
- **Strong typing**: IDs are newtypes (`ClientId`, `JobId`, …), states are enums, and schedules are a structured `Schedule` type (the original used regex strings).
- **Security**: argon2 password hashing; session tokens are stored only as SHA-256 hashes; `SameSite=Strict` cookies guard against CSRF;
  backup browsing compares canonicalized paths to prevent path traversal and symlink escapes;
  only admins can set ssh/rsync arguments that end up on a command line.

## Requirements

- Rust 1.88+ (edition 2024)
- PostgreSQL 13+
- `rsnapshot`, `rsync`, `ssh`, `ssh-keygen`, `tar`, `du`

## Quick start

```sh
# Development database (for production, see "Docker deployment" below)
docker compose up -d

cp config/elkarbackup.example.toml elkarbackup.toml   # adjust database_url, data_dir, etc.
export ELKARBACKUP_CONFIG=$PWD/elkarbackup.toml

cargo run -- migrate                         # create tables, default backup location and policy
echo 'a-strong-password' | cargo run -- create-admin --username admin --email admin@example.com
cargo run -- generate-keypair                # add the printed public key to each client's authorized_keys
cargo run -- serve                           # http://127.0.0.1:8080
```

To use cron instead of the built-in scheduler, run `serve --no-scheduler` and run `elkarbackup tick` every minute.

## Docker deployment

The `Dockerfile` uses a multi-stage build to produce a slim Debian image containing `rsnapshot`, `rsync`, `ssh` and `tini`;
`deploy/docker-compose.yml` starts both PostgreSQL and ElkarBackup.

```sh
cd deploy
cp .env.example .env        # set POSTGRES_PASSWORD, TZ and the data directory
vi elkarbackup.toml         # public_url, [mail], secure_cookies, etc.
docker compose up -d --build

# Create the first administrator (password is read from stdin)
echo 'a-strong-password' | docker compose exec -T elkarbackup \
    elkarbackup create-admin --username admin --email admin@example.com

# Get the public key and add it to each client's ~/.ssh/authorized_keys
docker compose exec elkarbackup elkarbackup generate-keypair --if-missing
```

- On first start the container creates the tables, the default backup location and policy, and an SSH key (`/var/lib/elkarbackup/ssh/id_ed25519`).
- `/var/lib/elkarbackup` (keys, scripts, logs, backups) lives in a volume named `elkarbackup-data` by default;
  for large backups, set `ELKARBACKUP_DATA=/srv/elkarbackup` in `.env` to use a host directory instead.
- Schedules are evaluated in the container's `TZ`; set it to your actual time zone.
- The web interface is published on `127.0.0.1:8080` only by default. For remote access, put a reverse proxy that terminates HTTPS in front of it and set `secure_cookies = true`.
- The container runs as root so that backups keep the original file owners; `GET /healthz` serves the health check.
- On stop, the container receives SIGTERM and running jobs are aborted (`stop_grace_period: 30s`).

## Scripts

Scripts receive their context through environment variables. The variable names match the original application, so existing scripts work unchanged:

| Variable | Description |
|----------|-------------|
| `ELKARBACKUP_LEVEL` | `CLIENT` or `JOB` |
| `ELKARBACKUP_EVENT` | `PRE` or `POST` |
| `ELKARBACKUP_URL` | The client's source URL (`url:path` at job level) |
| `ELKARBACKUP_ID` | Client or job ID |
| `ELKARBACKUP_PATH` | The job's snapshot root directory (job level) |
| `ELKARBACKUP_STATUS` | `0` success, `1` backup failed, `2` pre scripts failed, `3` aborted |
| `ELKARBACKUP_CLIENT_NAME`, `ELKARBACKUP_JOB_NAME` | Names |
| `ELKARBACKUP_JOB_RUN_SIZE`, `ELKARBACKUP_JOB_STARTTIME`, `ELKARBACKUP_JOB_ENDTIME` | Statistics for this run (post job) |
| `ELKARBACKUP_CLIENT_TOTAL_SIZE`, `ELKARBACKUP_JOB_TOTAL_SIZE` | Disk usage (KiB) |
| `ELKARBACKUP_OWNER_EMAIL`, `ELKARBACKUP_RECIPIENT_LIST`, `ELKARBACKUP_SSH_ARGS` | Other |

If scripts need root privileges, set `backup.script_prefix = ["sudo", "-n"]`.

## API

Every endpoint requires HTTP Basic authentication; regular users only see their own clients.

| Method | Path |
|--------|------|
| `GET` `POST` | `/api/clients` |
| `GET` `PUT` `DELETE` | `/api/clients/{id}` |
| `GET` `POST` | `/api/jobs` (`POST` requires `client_id`) |
| `GET` `PUT` `DELETE` | `/api/jobs/{id}` |
| `POST` | `/api/jobs/{id}/run`, `/api/jobs/{id}/abort` |
| `GET` `POST` / `GET` `PUT` `DELETE` | `/api/policies`, `/api/policies/{id}` (writes are admin only) |
| `GET` `POST` / `GET` `PUT` `DELETE` | `/api/backup_locations`, `/api/backup_locations/{id}` (writes are admin only) |
| `GET` | `/api/scripts`, `/api/users` (admin), `/api/queue`, `/api/logs?page=&per_page=&client=&job=` |

```sh
curl -u admin:pass -H 'Content-Type: application/json' \
     -d '{"name":"web1","url":"root@web1"}' http://127.0.0.1:8080/api/clients
```

## Testing

Integration tests need PostgreSQL; `#[sqlx::test]` creates a separate database for each test.

```sh
docker compose up -d
export DATABASE_URL=postgres://elkar:elkar@localhost:55432/elkarbackup
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Differences from the original

- A new schema from scratch; not compatible with the old database
- The interface is English only for now (templates live in `crates/elkar-web/templates`)
- System parameters are managed in the config file and can no longer be edited from the web interface
- Deleting a client or job does not delete its backup data on disk
- Not yet implemented: the repository backup script generator, the `authorized_keys` editor, and searching old backups across multiple backup locations

## License

BSD-3-Clause; see [LICENSE](LICENSE). This project is a rewrite of [ElkarBackup](https://github.com/elkarbackup/elkarbackup) (© Binovo IT Human Project, SL). It is an unofficial version and is not endorsed by Binovo or the ElkarBackup team.
