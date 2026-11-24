//! Scheduler state machine tests with a fake process runner.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{Local, TimeZone};
use elkar_core::client::{ClientSpec, ClientState};
use elkar_core::ids::{ClientId, JobId, ScriptId};
use elkar_core::job::{BackupResult, JobSpec, Recipient};
use elkar_core::location::LocationSpec;
use elkar_core::log::NotificationLevel;
use elkar_core::policy::{PolicySpec, RetainLevel, RetainRule};
use elkar_core::queue::QueueState;
use elkar_core::schedule::{Schedule, parse};
use elkar_core::script::ScriptSpec;
use elkar_engine::notify::Notification;
use elkar_engine::process::{CommandSpec, Outcome, ProcessRunner};
use elkar_engine::{Engine, EngineConfig, EngineError, Notifier, Scheduler};
use elkar_storage::{LogFilter, Store};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

/// Records every command and answers according to simple rules.
#[derive(Default)]
struct FakeRunner {
    calls: Mutex<Vec<CommandSpec>>,
    /// Script file names (e.g. `0001.script`) that exit with an error.
    failing_scripts: Mutex<Vec<String>>,
    rsnapshot_fails: Mutex<bool>,
    /// rsnapshot blocks until cancelled.
    rsnapshot_blocks: Mutex<bool>,
    running: AtomicUsize,
    max_running: AtomicUsize,
}

const STATS: &str = "Number of files: 3\nNumber of created files: 1\nNumber of deleted files: 0\nTotal transferred file size: 42 bytes\n";

impl FakeRunner {
    fn names(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|c| match c.program.rsplit('/').next().unwrap() {
                "rsnapshot" => format!("rsnapshot {}", c.args.last().unwrap()),
                other => other.to_owned(),
            })
            .collect()
    }

    fn script_status(&self, script: &str) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.program.ends_with(script))
            .map(|c| c.env["ELKARBACKUP_STATUS"].clone())
            .collect()
    }
}

#[async_trait]
impl ProcessRunner for FakeRunner {
    async fn run(&self, spec: &CommandSpec, cancel: &CancellationToken) -> std::io::Result<Outcome> {
        self.calls.lock().unwrap().push(spec.clone());
        let ok = |output: &str| Outcome::Finished {
            success: true,
            code: Some(0),
            output: output.into(),
        };
        let fail = Outcome::Finished {
            success: false,
            code: Some(1),
            output: "ssh: connection refused".into(),
        };

        if spec.program == "rsnapshot" {
            let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_running.fetch_max(now, Ordering::SeqCst);
            let outcome = if *self.rsnapshot_blocks.lock().unwrap() {
                cancel.cancelled().await;
                Outcome::Cancelled
            } else {
                tokio::time::sleep(Duration::from_millis(50)).await;
                if *self.rsnapshot_fails.lock().unwrap() {
                    fail
                } else {
                    ok(STATS)
                }
            };
            self.running.fetch_sub(1, Ordering::SeqCst);
            return Ok(outcome);
        }
        if spec.program == "du" {
            return Ok(ok("123\t/x\n"));
        }
        let failing = self.failing_scripts.lock().unwrap();
        if failing.iter().any(|s| spec.program.ends_with(s.as_str())) {
            return Ok(fail);
        }
        Ok(ok("script output"))
    }
}

#[derive(Default)]
struct RecordingNotifier(Mutex<Vec<Notification>>);

#[async_trait]
impl Notifier for RecordingNotifier {
    async fn send(&self, notification: Notification) -> Result<(), String> {
        self.0.lock().unwrap().push(notification);
        Ok(())
    }
}

struct Fixture {
    engine: Engine,
    runner: Arc<FakeRunner>,
    notifier: Arc<RecordingNotifier>,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new(pool: PgPool, max_parallel_jobs: u32) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut config = EngineConfig::rooted_at(dir.path());
        config.max_parallel_jobs = max_parallel_jobs;
        std::fs::create_dir_all(&config.scripts_dir).unwrap();
        let runner = Arc::new(FakeRunner::default());
        let notifier = Arc::new(RecordingNotifier::default());
        let engine = Engine {
            store: Store::from_pool(pool),
            config: Arc::new(config),
            runner: runner.clone(),
            notifier: notifier.clone(),
        };
        Self {
            engine,
            runner,
            notifier,
            _dir: dir,
        }
    }

    fn store(&self) -> &Store {
        &self.engine.store
    }

    async fn script(&self, name: &str) -> ScriptId {
        let id = self
            .store()
            .scripts()
            .create(&ScriptSpec {
                name: name.into(),
                is_client_pre: true,
                is_client_post: true,
                is_job_pre: true,
                is_job_post: true,
                ..ScriptSpec::default()
            })
            .await
            .unwrap();
        std::fs::write(
            self.engine.config.scripts_dir.join(format!("{}.script", id.padded())),
            "#!/bin/sh\n",
        )
        .unwrap();
        id
    }

    async fn policy(&self, name: &str, times: &str) -> elkar_core::ids::PolicyId {
        self.store()
            .policies()
            .create(&PolicySpec {
                name: name.into(),
                retains: vec![RetainRule {
                    level: RetainLevel::Daily,
                    count: 7,
                    schedule: Schedule {
                        times: parse::times(times).unwrap(),
                        ..Schedule::default()
                    },
                }],
                ..PolicySpec::default()
            })
            .await
            .unwrap()
    }

    async fn client(&self, name: &str, pre: Vec<ScriptId>, post: Vec<ScriptId>) -> ClientId {
        self.store()
            .clients()
            .create(&ClientSpec {
                name: name.into(),
                description: String::new(),
                url: String::new(),
                is_active: true,
                quota_kb: None,
                owner_id: None,
                ssh_args: String::new(),
                rsync_short_args: String::new(),
                rsync_long_args: String::new(),
                max_parallel_jobs: 2,
                pre_scripts: pre,
                post_scripts: post,
            })
            .await
            .unwrap()
    }

    async fn job(&self, client: ClientId, pre: Vec<ScriptId>, post: Vec<ScriptId>) -> JobId {
        let policy = self.policy(&format!("policy {client}"), "21:00").await;
        let location = self
            .store()
            .locations()
            .create(&LocationSpec {
                name: format!("loc {client}"),
                host: None,
                directory: self
                    .engine
                    .config
                    .tmp_dir
                    .parent()
                    .unwrap()
                    .join("backups")
                    .display()
                    .to_string(),
                max_parallel_jobs: 5,
            })
            .await
            .unwrap();
        self.store()
            .jobs()
            .create(
                client,
                &JobSpec {
                    name: "etc".into(),
                    description: String::new(),
                    path: "/etc".into(),
                    is_active: true,
                    policy_id: policy,
                    location_id: location,
                    include: None,
                    exclude: None,
                    use_local_permissions: true,
                    notify_to: vec![Recipient::Email],
                    notification_email: Some("ops@example.com".into()),
                    min_notification_level: NotificationLevel::Error,
                    priority: 1,
                    pre_scripts: pre,
                    post_scripts: post,
                },
            )
            .await
            .unwrap()
    }

    async fn run(&self) {
        let mut scheduler = Scheduler::start(self.engine.clone()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(20), scheduler.run_until_idle())
            .await
            .expect("scheduler went idle")
            .unwrap();
    }
}

#[sqlx::test(migrator = "elkar_storage::MIGRATOR")]
async fn full_cycle_runs_scripts_around_backup(pool: PgPool) {
    let f = Fixture::new(pool, 2);
    let client_script = f.script("client").await;
    let job_script = f.script("job").await;
    let client = f.client("c", vec![client_script], vec![client_script]).await;
    let job = f.job(client, vec![job_script], vec![job_script]).await;

    f.engine.enqueue_now(client, job, Some("admin")).await.unwrap();
    f.run().await;

    assert_eq!(
        f.runner.names(),
        vec![
            "0001.script",
            "0002.script",
            "rsnapshot daily",
            "du",
            "0002.script",
            "0001.script"
        ]
    );
    let stored = f.store().jobs().get(job).await.unwrap();
    assert_eq!(stored.last_result, Some(BackupResult::Ok));
    assert_eq!(stored.disk_usage_kb, 123);
    assert!(f.store().queue().list().await.unwrap().is_empty());
    assert_eq!(f.store().clients().get(client).await.unwrap().state, ClientState::Idle);

    let post_env = f.runner.calls.lock().unwrap()[4].env.clone();
    assert_eq!(post_env["ELKARBACKUP_JOB_RUN_SIZE"], "42 bytes");
    assert_eq!(post_env["ELKARBACKUP_LEVEL"], "JOB");
    // Nothing reached the error threshold, so nobody is notified.
    assert!(f.notifier.0.lock().unwrap().is_empty());
}

#[sqlx::test(migrator = "elkar_storage::MIGRATOR")]
async fn failing_client_pre_script_skips_jobs(pool: PgPool) {
    let f = Fixture::new(pool, 2);
    let script = f.script("pre").await;
    f.runner
        .failing_scripts
        .lock()
        .unwrap()
        .push(format!("{}.script", script.padded()));
    let client = f.client("c", vec![script], vec![]).await;
    let job = f.job(client, vec![], vec![]).await;

    f.engine.enqueue_now(client, job, None).await.unwrap();
    f.run().await;

    assert_eq!(f.runner.names(), vec!["0001.script"]);
    assert!(f.store().queue().list().await.unwrap().is_empty());
    assert_eq!(f.store().jobs().get(job).await.unwrap().last_result, None);
    let errors = f
        .store()
        .logs()
        .list(
            &LogFilter {
                min_level: Some(elkar_core::log::LogLevel::Error),
                ..LogFilter::default()
            },
            1,
            50,
        )
        .await
        .unwrap();
    assert!(
        errors
            .items
            .iter()
            .any(|r| r.message.contains("client pre scripts failed"))
    );
}

#[sqlx::test(migrator = "elkar_storage::MIGRATOR")]
async fn failed_backup_marks_job_and_notifies(pool: PgPool) {
    let f = Fixture::new(pool, 2);
    *f.runner.rsnapshot_fails.lock().unwrap() = true;
    let script = f.script("post").await;
    let client = f.client("c", vec![], vec![]).await;
    let job = f.job(client, vec![], vec![script]).await;

    f.engine.enqueue_now(client, job, None).await.unwrap();
    f.run().await;

    assert_eq!(
        f.store().jobs().get(job).await.unwrap().last_result,
        Some(BackupResult::Failed)
    );
    assert_eq!(f.runner.script_status("0001.script"), vec!["1"]);
    let sent = f.notifier.0.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, vec!["ops@example.com"]);
    assert!(sent[0].body.contains("ssh: connection refused"), "{}", sent[0].body);

    let failed_logs = std::fs::read_dir(f.engine.config.job_log_dir()).unwrap().count();
    assert_eq!(failed_logs, 1);
}

#[sqlx::test(migrator = "elkar_storage::MIGRATOR")]
async fn abort_cancels_running_backup_and_runs_post_scripts(pool: PgPool) {
    let f = Fixture::new(pool, 2);
    *f.runner.rsnapshot_blocks.lock().unwrap() = true;
    let script = f.script("post").await;
    let client = f.client("c", vec![], vec![]).await;
    let job = f.job(client, vec![], vec![script]).await;
    f.engine.enqueue_now(client, job, None).await.unwrap();

    let engine = f.engine.clone();
    let scheduler = tokio::spawn(async move {
        let mut s = Scheduler::start(engine).await.unwrap();
        s.run_until_idle().await.unwrap();
    });
    // Wait for the backup to start, then abort it.
    for _ in 0..100 {
        let item = f.store().queue().get_by_job(job).await.unwrap();
        if item.is_some_and(|i| i.state == QueueState::Running) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(f.engine.request_abort(client, job, "admin").await.unwrap());
    tokio::time::timeout(Duration::from_secs(20), scheduler)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(f.runner.script_status("0001.script"), vec!["3"]);
    assert_eq!(f.store().jobs().get(job).await.unwrap().last_result, None);
    assert!(f.store().queue().list().await.unwrap().is_empty());
}

#[sqlx::test(migrator = "elkar_storage::MIGRATOR")]
async fn global_limit_serialises_jobs(pool: PgPool) {
    let f = Fixture::new(pool, 1);
    for name in ["a", "b", "c"] {
        let client = f.client(name, vec![], vec![]).await;
        let job = f.job(client, vec![], vec![]).await;
        f.engine.enqueue_now(client, job, None).await.unwrap();
    }
    f.run().await;

    assert_eq!(
        f.runner.names().iter().filter(|n| n.starts_with("rsnapshot")).count(),
        3
    );
    assert_eq!(f.runner.max_running.load(Ordering::SeqCst), 1);
}

#[sqlx::test(migrator = "elkar_storage::MIGRATOR")]
async fn enqueues_only_on_scheduled_minutes(pool: PgPool) {
    let f = Fixture::new(pool, 1);
    let client = f.client("c", vec![], vec![]).await;
    let job = f.job(client, vec![], vec![]).await;
    let scheduler = Scheduler::start(f.engine.clone()).await.unwrap();

    let at = |h, m| Local.with_ymd_and_hms(2024, 5, 6, h, m, 30).unwrap();
    assert_eq!(scheduler.enqueue_scheduled(at(21, 1)).await.unwrap(), 0);
    assert_eq!(scheduler.enqueue_scheduled(at(21, 0)).await.unwrap(), 1);
    assert_eq!(scheduler.enqueue_scheduled(at(21, 0)).await.unwrap(), 0);

    let item = f.store().queue().get_by_job(job).await.unwrap().unwrap();
    assert_eq!(
        item.enqueued_at,
        at(21, 0).with_timezone(&chrono::Utc) - chrono::Duration::seconds(30)
    );
}

#[sqlx::test(migrator = "elkar_storage::MIGRATOR")]
async fn only_one_scheduler_at_a_time(pool: PgPool) {
    let f = Fixture::new(pool, 1);
    let first = Scheduler::start(f.engine.clone()).await.unwrap();
    assert!(matches!(
        Scheduler::start(f.engine.clone()).await,
        Err(EngineError::SchedulerBusy)
    ));
    drop(first);
    // The lock is released once the holding connection closes.
    let mut acquired = false;
    for _ in 0..50 {
        if Scheduler::start(f.engine.clone()).await.is_ok() {
            acquired = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(acquired);
}
