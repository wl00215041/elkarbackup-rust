CREATE TABLE users (
    id             BIGSERIAL PRIMARY KEY,
    username       TEXT        NOT NULL UNIQUE,
    email          TEXT        NOT NULL,
    password_hash  TEXT        NOT NULL,
    role           TEXT        NOT NULL CHECK (role IN ('admin', 'user')),
    is_active      BOOLEAN     NOT NULL DEFAULT TRUE,
    lines_per_page INTEGER     NOT NULL DEFAULT 20 CHECK (lines_per_page > 0),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE sessions (
    token_hash TEXT PRIMARY KEY,
    user_id    BIGINT      NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    expires_at TIMESTAMPTZ NOT NULL,
    -- One-shot message shown on the next rendered page.
    flash      TEXT
);
CREATE INDEX sessions_expires_at_idx ON sessions (expires_at);

CREATE TABLE backup_locations (
    id                BIGSERIAL PRIMARY KEY,
    name              TEXT    NOT NULL UNIQUE,
    host              TEXT,
    directory         TEXT    NOT NULL,
    max_parallel_jobs INTEGER NOT NULL DEFAULT 1 CHECK (max_parallel_jobs > 0)
);

CREATE TABLE policies (
    id          BIGSERIAL PRIMARY KEY,
    name        TEXT    NOT NULL UNIQUE,
    description TEXT    NOT NULL DEFAULT '',
    include     TEXT    NOT NULL DEFAULT '',
    exclude     TEXT    NOT NULL DEFAULT '',
    sync_first  BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE TABLE policy_retains (
    policy_id     BIGINT     NOT NULL REFERENCES policies (id) ON DELETE CASCADE,
    level         TEXT       NOT NULL CHECK (level IN ('hourly', 'daily', 'weekly', 'monthly', 'yearly')),
    count         INTEGER    NOT NULL CHECK (count > 0),
    times         TIME[]     NOT NULL,
    days_of_week  SMALLINT[] NOT NULL DEFAULT '{}',
    days_of_month SMALLINT[] NOT NULL DEFAULT '{}',
    months        SMALLINT[] NOT NULL DEFAULT '{}',
    PRIMARY KEY (policy_id, level)
);

CREATE TABLE scripts (
    id             BIGSERIAL PRIMARY KEY,
    name           TEXT        NOT NULL UNIQUE,
    description    TEXT        NOT NULL DEFAULT '',
    is_client_pre  BOOLEAN     NOT NULL DEFAULT FALSE,
    is_client_post BOOLEAN     NOT NULL DEFAULT FALSE,
    is_job_pre     BOOLEAN     NOT NULL DEFAULT FALSE,
    is_job_post    BOOLEAN     NOT NULL DEFAULT FALSE,
    last_updated   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE clients (
    id                BIGSERIAL PRIMARY KEY,
    name              TEXT    NOT NULL UNIQUE,
    description       TEXT    NOT NULL DEFAULT '',
    url               TEXT    NOT NULL DEFAULT '',
    is_active         BOOLEAN NOT NULL DEFAULT TRUE,
    quota_kb          BIGINT CHECK (quota_kb >= 0),
    owner_id          BIGINT REFERENCES users (id) ON DELETE SET NULL,
    ssh_args          TEXT    NOT NULL DEFAULT '',
    rsync_short_args  TEXT    NOT NULL DEFAULT '',
    rsync_long_args   TEXT    NOT NULL DEFAULT '',
    max_parallel_jobs INTEGER NOT NULL DEFAULT 1 CHECK (max_parallel_jobs > 0),
    state             TEXT    NOT NULL DEFAULT 'idle'
        CHECK (state IN ('idle', 'pre_scripts', 'ready', 'post_scripts', 'error')),
    cycle_started_at  TIMESTAMPTZ
);

CREATE TABLE client_scripts (
    client_id BIGINT NOT NULL REFERENCES clients (id) ON DELETE CASCADE,
    script_id BIGINT NOT NULL REFERENCES scripts (id) ON DELETE RESTRICT,
    phase     TEXT   NOT NULL CHECK (phase IN ('PRE', 'POST')),
    PRIMARY KEY (client_id, script_id, phase)
);

CREATE TABLE jobs (
    id                     BIGSERIAL PRIMARY KEY,
    client_id              BIGINT  NOT NULL REFERENCES clients (id) ON DELETE CASCADE,
    name                   TEXT    NOT NULL,
    description            TEXT    NOT NULL DEFAULT '',
    path                   TEXT    NOT NULL,
    is_active              BOOLEAN NOT NULL DEFAULT TRUE,
    policy_id              BIGINT  NOT NULL REFERENCES policies (id) ON DELETE RESTRICT,
    location_id            BIGINT  NOT NULL REFERENCES backup_locations (id) ON DELETE RESTRICT,
    include                TEXT,
    exclude                TEXT,
    use_local_permissions  BOOLEAN NOT NULL DEFAULT TRUE,
    notify_to              TEXT[]  NOT NULL DEFAULT '{owner}',
    notification_email     TEXT,
    min_notification_level TEXT    NOT NULL DEFAULT 'error'
        CHECK (min_notification_level IN ('all', 'info', 'warning', 'error', 'none')),
    priority               INTEGER NOT NULL DEFAULT 2147483647,
    disk_usage_kb          BIGINT  NOT NULL DEFAULT 0,
    last_result            TEXT CHECK (last_result IN ('ok', 'failed')),
    token                  TEXT UNIQUE,
    UNIQUE (client_id, name)
);
CREATE INDEX jobs_policy_id_idx ON jobs (policy_id);

CREATE TABLE job_scripts (
    job_id    BIGINT NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    script_id BIGINT NOT NULL REFERENCES scripts (id) ON DELETE RESTRICT,
    phase     TEXT   NOT NULL CHECK (phase IN ('PRE', 'POST')),
    PRIMARY KEY (job_id, script_id, phase)
);

CREATE TABLE queue (
    id            BIGSERIAL PRIMARY KEY,
    job_id        BIGINT      NOT NULL UNIQUE REFERENCES jobs (id) ON DELETE CASCADE,
    enqueued_at   TIMESTAMPTZ NOT NULL,
    running_since TIMESTAMPTZ,
    priority      INTEGER     NOT NULL,
    state         TEXT        NOT NULL DEFAULT 'queued'
        CHECK (state IN ('queued', 'waiting_client', 'pre_scripts', 'running', 'post_scripts')),
    aborted       BOOLEAN     NOT NULL DEFAULT FALSE,
    run_stats     JSONB
);

-- Log records outlive the entities they mention, hence no foreign keys.
CREATE TABLE log_records (
    id         BIGSERIAL PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    level      SMALLINT    NOT NULL,
    source     TEXT        NOT NULL,
    message    TEXT        NOT NULL,
    client_id  BIGINT,
    job_id     BIGINT,
    user_name  TEXT,
    log_file   TEXT
);
CREATE INDEX log_records_created_at_idx ON log_records (created_at);
CREATE INDEX log_records_subject_idx ON log_records (client_id, job_id, id DESC);
