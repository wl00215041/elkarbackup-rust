//! The scheduler state machine.
//!
//! Queue items move `queued → waiting_client → pre_scripts → running →
//! post_scripts → (removed)`. A client runs its pre scripts once before its
//! first waiting job (`idle → pre_scripts → ready`) and its post scripts
//! after its last one (`ready → post_scripts → idle`).
//!
//! [`Scheduler::step`] performs one non-blocking pass over that state; job
//! and script executions run as tasks whose completion is observed on the
//! next pass. Only one scheduler may run at a time, enforced with a
//! PostgreSQL advisory lock held for the scheduler's lifetime.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, Local, Timelike, Utc};
use elkar_core::client::{Client, ClientState};
use elkar_core::ids::{ClientId, JobId, LocationId};
use elkar_core::log::{LogLevel, NewLogRecord, Subject};
use elkar_core::queue::QueueState;
use elkar_core::script::Phase;
use elkar_storage::{AdvisoryLock, ClientFilter, QueueEntry};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::execution::{self, QueuedJob};
use crate::{Engine, EngineError, EngineResult, STATUS_SOURCE};

/// Advisory lock key shared by every scheduler instance ("ELKARBAK").
const LOCK_KEY: i64 = 0x454C_4B41_5242_414B;
/// Pause between passes while waiting for running tasks.
const POLL: Duration = Duration::from_secs(1);

enum Finished {
    ClientScripts { client: ClientId, phase: Phase, ok: bool },
    Job { job: JobId },
}

pub struct Scheduler {
    engine: Engine,
    /// Ensures a single scheduler; released when dropped.
    _lock: AdvisoryLock,
    tasks: JoinSet<Finished>,
    running_jobs: HashMap<JobId, (ClientId, CancellationToken)>,
    busy_clients: HashSet<ClientId>,
}

impl Scheduler {
    /// Takes the scheduler lock and resets state left over by a previous
    /// scheduler that did not finish.
    pub async fn start(engine: Engine) -> EngineResult<Self> {
        let lock = engine
            .store
            .try_lock(LOCK_KEY)
            .await?
            .ok_or(EngineError::SchedulerBusy)?;

        let scheduler = Self {
            engine,
            _lock: lock,
            tasks: JoinSet::new(),
            running_jobs: HashMap::new(),
            busy_clients: HashSet::new(),
        };
        scheduler.reset_state().await?;
        Ok(scheduler)
    }

    async fn reset_state(&self) -> EngineResult<()> {
        let store = &self.engine.store;
        for job_id in store.queue().reset_in_flight().await? {
            if let Ok(job) = store.jobs().get(job_id).await {
                self.record(
                    LogLevel::Warning,
                    "Job reset after scheduler restart",
                    Subject::Job(job.client_id, job_id),
                )
                .await;
            }
        }
        store.clients().reset_states().await?;
        Ok(())
    }

    /// Queues every job whose policy has a retain level due at `at`.
    /// Returns the number of jobs queued.
    pub async fn enqueue_scheduled(&self, at: DateTime<Local>) -> EngineResult<usize> {
        let store = &self.engine.store;
        let minute = at.with_second(0).and_then(|t| t.with_nanosecond(0)).unwrap_or(at);
        let due: Vec<_> = store
            .policies()
            .list()
            .await?
            .into_iter()
            .filter(|p| !p.scheduled_levels(minute.naive_local()).is_empty())
            .map(|p| p.id)
            .collect();
        if due.is_empty() {
            return Ok(0);
        }
        let mut queued = 0;
        for job in store.jobs().list_schedulable(&due).await? {
            if store
                .queue()
                .enqueue(job.id, minute.with_timezone(&Utc), job.spec.priority)
                .await?
            {
                queued += 1;
                self.record(LogLevel::Info, "Job queued", Subject::Job(job.client_id, job.id))
                    .await;
            }
        }
        Ok(queued)
    }

    /// Deletes log records older than the configured maximum age.
    pub async fn purge_old_logs(&self) -> EngineResult<u64> {
        let Some(max_age) = self.engine.config.max_log_age else {
            return Ok(0);
        };
        let age = chrono::Duration::from_std(max_age).unwrap_or(chrono::Duration::MAX);
        let cutoff = Utc::now().checked_sub_signed(age).unwrap_or(DateTime::<Utc>::MIN_UTC);
        let deleted = self.engine.store.logs().delete_older_than(cutoff).await?;
        self.engine.store.sessions().purge_expired().await?;
        Ok(deleted)
    }

    pub fn has_running_tasks(&self) -> bool {
        !self.tasks.is_empty()
    }

    /// One pass over the queue and the clients. Returns whether anything
    /// changed, so callers can tell when the scheduler has gone idle.
    pub async fn step(&mut self) -> EngineResult<bool> {
        let mut progress = self.reap_finished().await;

        let store = self.engine.store.clone();
        let clients: HashMap<ClientId, Client> = store
            .clients()
            .list(&ClientFilter::default())
            .await?
            .into_iter()
            .map(|c| (c.id, c))
            .collect();
        let location_limits: HashMap<LocationId, u32> = store
            .locations()
            .list()
            .await?
            .into_iter()
            .map(|l| (l.id, l.spec.max_parallel_jobs))
            .collect();
        let entries = store.queue().list().await?;
        let mut load = Load::from_entries(&entries);

        for entry in &entries {
            if !entry.active {
                continue;
            }
            let subject = Subject::Job(entry.client_id, entry.item.job_id);
            let client_state = clients.get(&entry.client_id).map(|c| c.state);
            match entry.item.state {
                QueueState::Queued | QueueState::WaitingClient if entry.item.aborted => {
                    store.queue().remove(entry.item.id).await?;
                    self.record(LogLevel::Warning, "Job aborted before it started", subject)
                        .await;
                    progress = true;
                }
                QueueState::Queued => {
                    let client_limit = clients.get(&entry.client_id).map_or(1, |c| c.spec.max_parallel_jobs);
                    let location_limit = location_limits.get(&entry.location_id).copied().unwrap_or(1);
                    if load.admits(
                        entry,
                        self.engine.config.max_parallel_jobs,
                        client_limit,
                        location_limit,
                    ) {
                        load.add(entry);
                        store
                            .queue()
                            .set_state(entry.item.id, QueueState::WaitingClient)
                            .await?;
                        progress = true;
                    }
                }
                QueueState::WaitingClient => match client_state {
                    Some(ClientState::Ready) => {
                        self.spawn_job(entry);
                        store.queue().set_state(entry.item.id, QueueState::PreScripts).await?;
                        progress = true;
                    }
                    Some(ClientState::Error) => {
                        store.queue().remove(entry.item.id).await?;
                        self.record(LogLevel::Error, "Job skipped: client pre scripts failed", subject)
                            .await;
                        progress = true;
                    }
                    _ => {}
                },
                QueueState::PreScripts | QueueState::Running | QueueState::PostScripts => {
                    match self.running_jobs.get(&entry.item.job_id) {
                        Some((_, token)) if entry.item.aborted && !token.is_cancelled() => {
                            token.cancel();
                            progress = true;
                        }
                        Some(_) => {}
                        None => {
                            // In flight without a task: re-run it from the start.
                            store.queue().set_state(entry.item.id, QueueState::Queued).await?;
                            progress = true;
                        }
                    }
                }
            }
        }

        let entries = store.queue().list().await?;
        for client in clients.values() {
            progress |= self.advance_client(client, &entries).await?;
        }
        Ok(progress)
    }

    async fn advance_client(&mut self, client: &Client, entries: &[QueueEntry]) -> EngineResult<bool> {
        if self.busy_clients.contains(&client.id) {
            return Ok(false);
        }
        let mine = || entries.iter().filter(|e| e.client_id == client.id);
        let clients = self.engine.store.clients();
        match client.state {
            ClientState::Idle if mine().any(|e| e.item.state == QueueState::WaitingClient) => {
                clients.set_state(client.id, ClientState::PreScripts).await?;
                self.spawn_client_scripts(client.id, Phase::Pre);
                Ok(true)
            }
            ClientState::Ready if mine().next().is_none() && !self.client_has_running_job(client.id) => {
                clients.set_state(client.id, ClientState::PostScripts).await?;
                self.spawn_client_scripts(client.id, Phase::Post);
                Ok(true)
            }
            ClientState::Error if mine().next().is_none() => {
                clients.set_state(client.id, ClientState::Idle).await?;
                Ok(true)
            }
            // Recover from states left without a task (e.g. after a crash).
            ClientState::PreScripts | ClientState::PostScripts => {
                clients.set_state(client.id, ClientState::Idle).await?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn client_has_running_job(&self, client: ClientId) -> bool {
        self.running_jobs.values().any(|(c, _)| *c == client)
    }

    fn spawn_job(&mut self, entry: &QueueEntry) {
        let token = CancellationToken::new();
        let queued = QueuedJob {
            queue_id: entry.item.id,
            job_id: entry.item.job_id,
            client_id: entry.client_id,
            enqueued_at: entry.item.enqueued_at,
        };
        self.running_jobs
            .insert(queued.job_id, (queued.client_id, token.clone()));
        let engine = self.engine.clone();
        self.tasks.spawn(async move {
            execution::run_job(engine, queued, token).await;
            Finished::Job { job: queued.job_id }
        });
    }

    fn spawn_client_scripts(&mut self, client: ClientId, phase: Phase) {
        self.busy_clients.insert(client);
        let engine = self.engine.clone();
        self.tasks.spawn(async move {
            let ok = execution::run_client_scripts(engine, client, phase, CancellationToken::new()).await;
            Finished::ClientScripts { client, phase, ok }
        });
    }

    /// Applies the results of every finished task without blocking.
    async fn reap_finished(&mut self) -> bool {
        let mut any = false;
        while let Some(result) = self.tasks.try_join_next() {
            self.handle(result).await;
            any = true;
        }
        any
    }

    async fn handle(&mut self, result: Result<Finished, tokio::task::JoinError>) {
        let finished = match result {
            Ok(finished) => finished,
            Err(err) => {
                // A panicking task loses its identity; the in-flight
                // recovery in `step` re-queues whatever it left behind.
                tracing::error!(error = %err, "scheduler task panicked");
                self.running_jobs.retain(|_, (_, token)| !token.is_cancelled());
                return;
            }
        };
        match finished {
            Finished::Job { job } => {
                self.running_jobs.remove(&job);
            }
            Finished::ClientScripts { client, phase, ok } => {
                self.busy_clients.remove(&client);
                let state = match (phase, ok) {
                    (Phase::Pre, true) => ClientState::Ready,
                    (Phase::Post, true) => ClientState::Idle,
                    (_, false) => ClientState::Error,
                };
                if let Err(err) = self.engine.store.clients().set_state(client, state).await {
                    tracing::error!(error = %err, %client, "could not update client state");
                }
                if !ok {
                    self.record(
                        LogLevel::Error,
                        format!("Client {phase} scripts failed"),
                        Subject::Client(client),
                    )
                    .await;
                }
            }
        }
    }

    /// Waits up to `max` for a task to finish.
    async fn wait(&mut self, max: Duration) {
        if self.tasks.is_empty() {
            tokio::time::sleep(max).await;
        } else if let Ok(Some(result)) = tokio::time::timeout(max, self.tasks.join_next()).await {
            self.handle(result).await;
        }
    }

    /// Processes the queue until nothing more can happen (`tick` mode).
    pub async fn run_until_idle(&mut self) -> EngineResult<()> {
        loop {
            if self.step().await? {
                continue;
            }
            if self.tasks.is_empty() {
                break;
            }
            self.wait(POLL).await;
        }
        let remaining = self.engine.store.queue().list().await?;
        if !remaining.is_empty() {
            tracing::warn!(
                count = remaining.len(),
                "jobs remain queued but cannot run with the current configuration"
            );
        }
        self.engine.store.clients().reset_states().await?;
        Ok(())
    }

    /// Runs until `shutdown` fires (daemon mode): enqueues scheduled jobs
    /// at every minute and keeps the queue moving.
    pub async fn run_forever(&mut self, shutdown: CancellationToken) {
        let mut last_minute = None;
        loop {
            let now = Local::now();
            let minute = (now.date_naive(), now.hour(), now.minute());
            if last_minute != Some(minute) {
                last_minute = Some(minute);
                if let Err(err) = self.enqueue_scheduled(now).await {
                    tracing::error!(error = %err, "could not enqueue scheduled jobs");
                }
                if now.minute() == 0
                    && let Err(err) = self.purge_old_logs().await
                {
                    tracing::error!(error = %err, "could not purge old logs");
                }
            }
            match self.step().await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(err) => tracing::error!(error = %err, "scheduler pass failed"),
            }
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = self.wait(POLL) => {}
            }
        }
        self.stop().await;
    }

    /// Cancels running jobs and waits for every task to wind down.
    async fn stop(&mut self) {
        for (_, token) in self.running_jobs.values() {
            token.cancel();
        }
        while let Some(result) = self.tasks.join_next().await {
            self.handle(result).await;
        }
    }

    async fn record(&self, level: LogLevel, message: impl Into<String>, subject: Subject) {
        self.engine
            .activity()
            .record(NewLogRecord::new(level, STATUS_SOURCE, message).about(subject))
            .await;
    }
}

/// Number of items past `queued`, globally and per client / location.
#[derive(Default)]
struct Load {
    global: u32,
    per_client: HashMap<ClientId, u32>,
    per_location: HashMap<LocationId, u32>,
}

impl Load {
    fn from_entries(entries: &[QueueEntry]) -> Self {
        let mut load = Self::default();
        for entry in entries.iter().filter(|e| e.active && e.item.state.is_active()) {
            load.add(entry);
        }
        load
    }

    fn add(&mut self, entry: &QueueEntry) {
        self.global += 1;
        *self.per_client.entry(entry.client_id).or_default() += 1;
        *self.per_location.entry(entry.location_id).or_default() += 1;
    }

    fn admits(&self, entry: &QueueEntry, global: u32, client: u32, location: u32) -> bool {
        self.global < global
            && self.per_client.get(&entry.client_id).copied().unwrap_or(0) < client
            && self.per_location.get(&entry.location_id).copied().unwrap_or(0) < location
    }
}
