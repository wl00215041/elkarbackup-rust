//! Execution of one queued job and of client level scripts. These run as
//! independent tasks spawned by the scheduler.

use chrono::{Local, Utc};
use elkar_core::client::Client;
use elkar_core::ids::{ClientId, JobId, QueueId};
use elkar_core::job::{BackupResult as JobResult, Job, Recipient};
use elkar_core::log::Subject;
use elkar_core::queue::{QueueState, RunStats};
use elkar_core::script::{Level, Phase};
use tokio_util::sync::CancellationToken;

use crate::activity::SubjectLog;
use crate::backup::{self, BackupPlan, BackupResult, BackupRunner};
use crate::notify::Notification;
use crate::scripts::{RunStatus, ScriptEnv, ScriptRunner};
use crate::{Engine, EngineResult, STATUS_SOURCE};

/// The queue item being executed.
#[derive(Debug, Clone, Copy)]
pub struct QueuedJob {
    pub queue_id: QueueId,
    pub job_id: JobId,
    pub client_id: ClientId,
    pub enqueued_at: chrono::DateTime<Utc>,
}

/// Runs pre job scripts, rsnapshot and post job scripts, then records the
/// result, removes the queue item and sends notifications.
pub async fn run_job(engine: Engine, queued: QueuedJob, cancel: CancellationToken) {
    let subject = Subject::Job(queued.client_id, queued.job_id);
    let mut log = engine.activity().for_subject(STATUS_SOURCE, subject);
    let result = execute(&engine, queued, &mut log, &cancel).await;

    match result {
        Ok(Some((job, client))) => notify(&engine, &job, &client, &log).await,
        Ok(None) => {}
        Err(err) => log.error(format!("Job execution failed: {err}")).await,
    }
    if let Err(err) = engine.store.queue().remove(queued.queue_id).await {
        tracing::error!(error = %err, job = %queued.job_id, "could not remove queue item");
    }
}

/// Returns the job and client when a notification should be considered.
async fn execute(
    engine: &Engine,
    queued: QueuedJob,
    log: &mut SubjectLog,
    cancel: &CancellationToken,
) -> EngineResult<Option<(Job, Client)>> {
    let store = &engine.store;
    let job = store.jobs().get(queued.job_id).await?;
    let client = store.clients().get(job.client_id).await?;
    let policy = store.policies().get(job.spec.policy_id).await?;
    let location = store.locations().get(job.spec.location_id).await?;
    let owner_email = match client.spec.owner_id {
        Some(owner) => store.users().get(owner).await.map(|u| u.spec.email).unwrap_or_default(),
        None => String::new(),
    };

    let levels = policy.levels_to_run(queued.enqueued_at.with_timezone(&Local).naive_local());
    if levels.is_empty() {
        log.warn(format!("Policy \"{}\" has no retain levels", policy.spec.name))
            .await;
        return Ok(None);
    }
    let plan = BackupPlan {
        job: &job,
        client: &client,
        policy: &policy,
        location: &location,
        levels,
    };
    let scripts = ScriptRunner {
        config: &engine.config,
        runner: engine.runner.as_ref(),
    };
    let env = |phase, status, stats: &RunStats| {
        ScriptEnv::new(Level::Job, phase, status)
            .set("URL", client.source_url(&job.spec.path))
            .set("ID", job.id)
            .set("PATH", plan.snapshot_root().display())
            .set("CLIENT_NAME", &client.spec.name)
            .set("JOB_NAME", &job.spec.name)
            .set("OWNER_EMAIL", &owner_email)
            .set("RECIPIENT_LIST", job.spec.notification_email.as_deref().unwrap_or(""))
            .set("JOB_TOTAL_SIZE", job.disk_usage_kb)
            .set("JOB_RUN_SIZE", &stats.transferred)
            .set("JOB_STARTTIME", stats.started_at)
            .set("JOB_ENDTIME", stats.ended_at)
            .set("SSH_ARGS", &client.spec.ssh_args)
    };

    // Pre job scripts.
    store.queue().set_state(queued.queue_id, QueueState::PreScripts).await?;
    let pre_scripts = store.scripts().for_job(job.id, Phase::Pre).await?;
    let pre_env = env(Phase::Pre, RunStatus::Ok, &RunStats::default());
    let pre_ok = scripts.run_all(pre_scripts, Phase::Pre, &pre_env, log, cancel).await;

    let (status, stats) = if !pre_ok {
        log.error("Pre job scripts failed: backup skipped").await;
        if !engine.config.post_on_pre_fail {
            store.jobs().set_last_result(job.id, JobResult::Failed).await?;
            return Ok(Some((job, client)));
        }
        (RunStatus::PreScriptsFailed, RunStats::default())
    } else if cancel.is_cancelled() {
        log.warn("Job aborted before the backup started").await;
        (RunStatus::Aborted, RunStats::default())
    } else {
        store.queue().set_state(queued.queue_id, QueueState::Running).await?;
        let runner = BackupRunner {
            config: &engine.config,
            runner: engine.runner.as_ref(),
        };
        match runner.run(&plan, log, cancel).await {
            BackupResult::Succeeded(stats) => (RunStatus::Ok, stats),
            BackupResult::Failed(stats) => (RunStatus::BackupFailed, stats),
            BackupResult::Cancelled => (RunStatus::Aborted, RunStats::default()),
        }
    };
    store.queue().set_run_stats(queued.queue_id, &stats).await?;

    if status != RunStatus::Aborted
        && engine.config.disk_usage_enabled
        && let Some(kb) = backup::disk_usage_kb(engine.runner.as_ref(), &plan.snapshot_root()).await
    {
        store.jobs().set_disk_usage(job.id, kb).await?;
    }

    // Post job scripts always run so they can release what pre scripts took,
    // but are not cancellable themselves.
    store
        .queue()
        .set_state(queued.queue_id, QueueState::PostScripts)
        .await?;
    let post_scripts = store.scripts().for_job(job.id, Phase::Post).await?;
    let post_env = env(Phase::Post, status, &stats);
    let post_ok = scripts
        .run_all(post_scripts, Phase::Post, &post_env, log, &CancellationToken::new())
        .await;
    if !post_ok {
        log.error("Post job scripts failed").await;
    }

    if status == RunStatus::Aborted {
        log.warn("Job aborted").await;
    } else if log.has_errors() {
        log.warn("Job finished with errors").await;
        store.jobs().set_last_result(job.id, JobResult::Failed).await?;
    } else {
        log.info("Job finished successfully").await;
        store.jobs().set_last_result(job.id, JobResult::Ok).await?;
    }
    Ok(Some((job, client)))
}

/// Emails the job's records that reach its minimum notification level.
async fn notify(engine: &Engine, job: &Job, client: &Client, log: &SubjectLog) {
    let level = job.spec.min_notification_level;
    let lines: Vec<String> = log
        .records()
        .iter()
        .filter(|r| level.accepts(r.level))
        .map(|r| format!("[{}] {}", r.level.as_str().to_uppercase(), r.message))
        .collect();
    if lines.is_empty() {
        return;
    }
    let recipients = recipients(engine, job, client).await;
    if recipients.is_empty() {
        return;
    }
    let link = format!(
        "{}{}",
        engine.config.public_url.trim_end_matches('/'),
        Subject::Job(client.id, job.id).path()
    );
    let notification = Notification {
        to: recipients,
        subject: format!(
            "[ElkarBackup] {} / {}: {}",
            client.spec.name,
            job.spec.name,
            if log.has_errors() { "failed" } else { "report" }
        ),
        body: format!(
            "Backup report for {}\n{link}\n\n{}\n",
            client.source_url(&job.spec.path),
            lines.join("\n")
        ),
    };
    if let Err(err) = engine.notifier.send(notification).await {
        engine
            .activity()
            .record(
                elkar_core::log::NewLogRecord::new(
                    elkar_core::log::LogLevel::Error,
                    STATUS_SOURCE,
                    format!("Could not send notification: {err}"),
                )
                .about(log.subject()),
            )
            .await;
    }
}

async fn recipients(engine: &Engine, job: &Job, client: &Client) -> Vec<String> {
    let mut to = Vec::new();
    for recipient in &job.spec.notify_to {
        let address = match recipient {
            Recipient::Admin => engine.store.users().primary_admin_email().await.ok().flatten(),
            Recipient::Owner => match client.spec.owner_id {
                Some(owner) => engine.store.users().get(owner).await.ok().map(|u| u.spec.email),
                None => None,
            },
            Recipient::Email => job.spec.notification_email.clone(),
        };
        if let Some(address) = address.filter(|a| !a.is_empty())
            && !to.contains(&address)
        {
            to.push(address);
        }
    }
    to
}

/// Runs a client's pre or post scripts; returns whether all succeeded.
pub async fn run_client_scripts(engine: Engine, client_id: ClientId, phase: Phase, cancel: CancellationToken) -> bool {
    let subject = Subject::Client(client_id);
    let mut log = engine.activity().for_subject(STATUS_SOURCE, subject);
    match client_scripts(&engine, client_id, phase, &mut log, &cancel).await {
        Ok(ok) => ok,
        Err(err) => {
            log.error(format!("Client {phase} scripts failed: {err}")).await;
            false
        }
    }
}

async fn client_scripts(
    engine: &Engine,
    client_id: ClientId,
    phase: Phase,
    log: &mut SubjectLog,
    cancel: &CancellationToken,
) -> EngineResult<bool> {
    let store = &engine.store;
    let client = store.clients().get(client_id).await?;
    let now = Utc::now();
    let (started, ended) = match phase {
        Phase::Pre => {
            store.clients().set_cycle_started(client_id, Some(now)).await?;
            (now.timestamp(), 0)
        }
        Phase::Post => {
            let started = store.clients().cycle_started(client_id).await?;
            store.clients().set_cycle_started(client_id, None).await?;
            (started.map_or(0, |s| s.timestamp()), now.timestamp())
        }
    };
    let env = ScriptEnv::new(Level::Client, phase, RunStatus::Ok)
        .set("URL", &client.spec.url)
        .set("ID", client.id)
        .set("CLIENT_NAME", &client.spec.name)
        .set("CLIENT_TOTAL_SIZE", store.clients().disk_usage_kb(client_id).await?)
        .set("CLIENT_STARTTIME", started)
        .set("CLIENT_ENDTIME", ended)
        .set("SSH_ARGS", &client.spec.ssh_args);
    let scripts = store.scripts().for_client(client_id, phase).await?;
    let runner = ScriptRunner {
        config: &engine.config,
        runner: engine.runner.as_ref(),
    };
    Ok(runner.run_all(scripts, phase, &env, log, cancel).await)
}
