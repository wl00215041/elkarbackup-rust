//! Integration tests against a real PostgreSQL. `#[sqlx::test]` creates a
//! fresh database per test from `DATABASE_URL` and runs the migrations.

use chrono::{Utc, Weekday};
use elkar_core::client::{ClientSpec, ClientState};
use elkar_core::ids::{LocationId, PolicyId, UserId};
use elkar_core::job::{BackupResult, JobSpec, Recipient};
use elkar_core::location::LocationSpec;
use elkar_core::log::{LogLevel, NewLogRecord, NotificationLevel, Subject};
use elkar_core::policy::{PolicySpec, RetainLevel, RetainRule};
use elkar_core::queue::{QueueState, RunStats};
use elkar_core::schedule::{Schedule, parse};
use elkar_core::script::{Phase, ScriptSpec};
use elkar_core::user::{Role, UserSpec};
use elkar_storage::{ClientFilter, LogFilter, StorageError, Store};
use sqlx::PgPool;

fn policy_spec(name: &str) -> PolicySpec {
    PolicySpec {
        name: name.into(),
        description: "d".into(),
        include: "/etc".into(),
        exclude: "*.tmp".into(),
        sync_first: true,
        retains: vec![
            RetainRule {
                level: RetainLevel::Daily,
                count: 7,
                schedule: Schedule {
                    times: parse::times("21:00").unwrap(),
                    ..Schedule::default()
                },
            },
            RetainRule {
                level: RetainLevel::Weekly,
                count: 4,
                schedule: Schedule {
                    times: parse::times("21:00").unwrap(),
                    days_of_week: vec![Weekday::Sun],
                    days_of_month: vec![1, 15],
                    months: vec![12],
                },
            },
        ],
    }
}

fn client_spec(name: &str, owner: Option<UserId>) -> ClientSpec {
    ClientSpec {
        name: name.into(),
        description: String::new(),
        url: "root@host".into(),
        is_active: true,
        quota_kb: None,
        owner_id: owner,
        ssh_args: String::new(),
        rsync_short_args: String::new(),
        rsync_long_args: String::new(),
        max_parallel_jobs: 1,
        pre_scripts: vec![],
        post_scripts: vec![],
    }
}

fn job_spec(policy: PolicyId, location: LocationId) -> JobSpec {
    JobSpec {
        name: "etc".into(),
        description: String::new(),
        path: "/etc".into(),
        is_active: true,
        policy_id: policy,
        location_id: location,
        include: None,
        exclude: Some("*.log".into()),
        use_local_permissions: true,
        notify_to: vec![Recipient::Owner, Recipient::Admin],
        notification_email: None,
        min_notification_level: NotificationLevel::Warning,
        priority: 5,
        pre_scripts: vec![],
        post_scripts: vec![],
    }
}

async fn location(store: &Store) -> LocationId {
    store
        .locations()
        .create(&LocationSpec {
            name: "Default".into(),
            host: None,
            directory: "/backups".into(),
            max_parallel_jobs: 2,
        })
        .await
        .unwrap()
}

#[sqlx::test(migrator = "elkar_storage::MIGRATOR")]
async fn policy_round_trips_retains_and_calendar(pool: PgPool) {
    let store = Store::from_pool(pool);
    let id = store.policies().create(&policy_spec("p")).await.unwrap();
    let policy = store.policies().get(id).await.unwrap();
    assert_eq!(policy.spec, policy_spec("p"));

    let mut changed = policy_spec("renamed");
    changed.retains.truncate(1);
    store.policies().update(id, &changed).await.unwrap();
    let listed = store.policies().list().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].spec, changed);
}

#[sqlx::test(migrator = "elkar_storage::MIGRATOR")]
async fn duplicate_names_are_conflicts(pool: PgPool) {
    let store = Store::from_pool(pool);
    store.policies().create(&policy_spec("p")).await.unwrap();
    let err = store.policies().create(&policy_spec("p")).await.unwrap_err();
    assert!(matches!(err, StorageError::Conflict(_)), "{err:?}");
}

#[sqlx::test(migrator = "elkar_storage::MIGRATOR")]
async fn clients_jobs_and_scripts(pool: PgPool) {
    let store = Store::from_pool(pool);
    let user = store
        .users()
        .create(
            &UserSpec {
                username: "ann".into(),
                email: "ann@example.com".into(),
                role: Role::User,
                is_active: true,
                lines_per_page: 20,
            },
            "hash",
        )
        .await
        .unwrap();
    let script = store
        .scripts()
        .create(&ScriptSpec {
            name: "s".into(),
            is_client_pre: true,
            is_job_post: true,
            ..ScriptSpec::default()
        })
        .await
        .unwrap();

    let mut spec = client_spec("web", Some(user));
    spec.pre_scripts = vec![script];
    let client = store.clients().create(&spec).await.unwrap();
    store.clients().create(&client_spec("other", None)).await.unwrap();

    let loaded = store.clients().get(client).await.unwrap();
    assert_eq!(loaded.spec, spec);
    assert_eq!(loaded.state, ClientState::Idle);
    let owned = store
        .clients()
        .list(&ClientFilter {
            owner: Some(user),
            ..ClientFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(owned.len(), 1);

    let policy = store.policies().create(&policy_spec("p")).await.unwrap();
    let loc = location(&store).await;
    let mut jspec = job_spec(policy, loc);
    jspec.post_scripts = vec![script];
    let job = store.jobs().create(client, &jspec).await.unwrap();
    let loaded_job = store.jobs().get(job).await.unwrap();
    assert_eq!(loaded_job.spec, jspec);
    assert_eq!(loaded_job.last_result, None);

    store.jobs().set_last_result(job, BackupResult::Ok).await.unwrap();
    store.jobs().set_disk_usage(job, 42).await.unwrap();
    assert_eq!(store.clients().disk_usage_kb(client).await.unwrap(), 42);
    assert_eq!(store.scripts().for_job(job, Phase::Post).await.unwrap()[0].id, script);

    let err = store.scripts().delete(script).await.unwrap_err();
    assert!(matches!(err, StorageError::InUse(_)), "{err:?}");
    let err = store.policies().delete(policy).await.unwrap_err();
    assert!(matches!(err, StorageError::InUse(_)), "{err:?}");

    assert_eq!(store.jobs().list_schedulable(&[policy]).await.unwrap().len(), 1);
    store.clients().delete(client).await.unwrap();
    assert!(matches!(store.jobs().get(job).await, Err(StorageError::NotFound)));
}

#[sqlx::test(migrator = "elkar_storage::MIGRATOR")]
async fn queue_lifecycle(pool: PgPool) {
    let store = Store::from_pool(pool);
    let policy = store.policies().create(&policy_spec("p")).await.unwrap();
    let loc = location(&store).await;
    let client = store.clients().create(&client_spec("c", None)).await.unwrap();
    let job = store.jobs().create(client, &job_spec(policy, loc)).await.unwrap();

    assert!(store.queue().enqueue(job, Utc::now(), 5).await.unwrap());
    assert!(!store.queue().enqueue(job, Utc::now(), 5).await.unwrap());

    let entry = store.queue().list().await.unwrap().remove(0);
    assert_eq!(entry.client_id, client);
    assert_eq!(entry.item.state, QueueState::Queued);

    store
        .queue()
        .set_state(entry.item.id, QueueState::PreScripts)
        .await
        .unwrap();
    let stats = RunStats {
        transferred: "1 bytes".into(),
        started_at: 1,
        ended_at: 2,
    };
    store.queue().set_run_stats(entry.item.id, &stats).await.unwrap();
    assert_eq!(store.queue().run_stats(entry.item.id).await.unwrap(), Some(stats));

    let item = store.queue().get_by_job(job).await.unwrap().unwrap();
    assert!(item.running_since.is_some());

    assert_eq!(store.queue().reset_in_flight().await.unwrap(), vec![job]);
    assert!(store.queue().request_abort(job).await.unwrap());
    assert!(store.queue().is_aborted(item.id).await.unwrap());
    store.queue().remove(item.id).await.unwrap();
    assert!(store.queue().list().await.unwrap().is_empty());
    assert!(!store.queue().request_abort(job).await.unwrap());
}

#[sqlx::test(migrator = "elkar_storage::MIGRATOR")]
async fn logs_filter_and_paginate(pool: PgPool) {
    let store = Store::from_pool(pool);
    let subject = Subject::Job(1.into(), 2.into());
    for i in 0..5 {
        let level = if i % 2 == 0 { LogLevel::Info } else { LogLevel::Error };
        store
            .logs()
            .insert(&NewLogRecord::new(level, "Scheduler", format!("message {i}")).about(subject))
            .await
            .unwrap();
    }
    store
        .logs()
        .insert(&NewLogRecord::new(LogLevel::Info, "Web", "login"))
        .await
        .unwrap();

    let all = store.logs().list(&LogFilter::default(), 1, 4).await.unwrap();
    assert_eq!(all.total, 6);
    assert_eq!(all.items.len(), 4);
    assert_eq!(all.page_count(), 2);
    assert_eq!(all.items[0].message, "login");

    let errors = store
        .logs()
        .list(
            &LogFilter {
                min_level: Some(LogLevel::Error),
                ..LogFilter::default()
            },
            1,
            10,
        )
        .await
        .unwrap();
    assert_eq!(errors.total, 2);
    assert_eq!(errors.items[0].subject(), Some(subject));

    let latest = store.logs().latest_status_per_job("Scheduler").await.unwrap();
    assert_eq!(latest.len(), 1);
    assert_eq!(latest[0].message, "message 4");

    let deleted = store
        .logs()
        .delete_older_than(Utc::now() + chrono::Duration::hours(1))
        .await
        .unwrap();
    assert_eq!(deleted, 6);
}

#[sqlx::test(migrator = "elkar_storage::MIGRATOR")]
async fn sessions_expire_and_require_active_user(pool: PgPool) {
    let store = Store::from_pool(pool);
    let spec = UserSpec {
        username: "root".into(),
        email: "root@example.com".into(),
        role: Role::Admin,
        is_active: true,
        lines_per_page: 20,
    };
    let user = store.users().create(&spec, "hash").await.unwrap();
    let future = Utc::now() + chrono::Duration::hours(1);
    store.sessions().create("live", user, future).await.unwrap();
    store
        .sessions()
        .create("old", user, Utc::now() - chrono::Duration::hours(1))
        .await
        .unwrap();

    assert_eq!(store.sessions().user_for("live").await.unwrap(), Some(user));
    assert_eq!(store.sessions().user_for("old").await.unwrap(), None);
    assert_eq!(store.sessions().purge_expired().await.unwrap(), 1);

    store
        .users()
        .update(
            user,
            &UserSpec {
                is_active: false,
                ..spec
            },
        )
        .await
        .unwrap();
    assert_eq!(store.sessions().user_for("live").await.unwrap(), None);
}
