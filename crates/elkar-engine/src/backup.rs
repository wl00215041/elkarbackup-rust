//! Running rsnapshot for one job.

use std::path::{Path, PathBuf};

use chrono::Utc;
use elkar_core::client::Client;
use elkar_core::job::Job;
use elkar_core::location::BackupLocation;
use elkar_core::log::{LogLevel, NewLogRecord};
use elkar_core::policy::{Policy, RetainLevel};
use elkar_core::queue::RunStats;
use elkar_core::rsnapshot::{RsnapshotConfig, TransferStats, split_patterns, summarize_failure};
use tokio_util::sync::CancellationToken;

use crate::activity::SubjectLog;
use crate::process::{CommandSpec, Outcome, ProcessRunner};
use crate::scripts::excerpt;
use crate::{EngineConfig, EngineError, EngineResult};

/// Everything needed to back up one job.
pub struct BackupPlan<'a> {
    pub job: &'a Job,
    pub client: &'a Client,
    pub policy: &'a Policy,
    pub location: &'a BackupLocation,
    pub levels: Vec<RetainLevel>,
}

impl BackupPlan<'_> {
    pub fn snapshot_root(&self) -> PathBuf {
        self.location.snapshot_root(self.client.id, self.job.id)
    }

    fn file_stem(&self) -> String {
        format!("{}_{}", self.client.id.padded(), self.job.id.padded())
    }

    fn tmp_log(&self, config: &EngineConfig) -> PathBuf {
        config
            .log_dir
            .join(format!("tmp-c{}j{}.log", self.client.id.padded(), self.job.id.padded()))
    }

    /// The rsnapshot configuration this plan runs with.
    pub fn rsnapshot_config(&self, config: &EngineConfig) -> RsnapshotConfig {
        RsnapshotConfig {
            tools: config.tools.clone(),
            snapshot_root: self.snapshot_root(),
            retains: self.policy.retains().iter().map(|r| (r.level, r.count)).collect(),
            log_file: self.tmp_log(config),
            lock_file: config.tmp_dir.join(format!("rsnapshot.{}.pid", self.file_stem())),
            rsync_short_args: self.client.spec.rsync_short_args.clone(),
            rsync_long_args: self.client.spec.rsync_long_args.clone(),
            use_local_permissions: self.job.spec.use_local_permissions,
            identity_file: config.ssh_key.clone(),
            ssh_args: self.client.spec.ssh_args.clone(),
            includes: split_patterns(self.job.include(self.policy)),
            excludes: split_patterns(self.job.exclude(self.policy)),
            sync_first: self.policy.spec.sync_first,
            source: self.client.source_url(&self.job.spec.path),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupResult {
    Succeeded(RunStats),
    Failed(RunStats),
    Cancelled,
}

pub struct BackupRunner<'a> {
    pub config: &'a EngineConfig,
    pub runner: &'a dyn ProcessRunner,
}

impl BackupRunner<'_> {
    pub async fn run(&self, plan: &BackupPlan<'_>, log: &mut SubjectLog, cancel: &CancellationToken) -> BackupResult {
        let started_at = Utc::now().timestamp();
        let stats = |transferred: String| RunStats {
            transferred,
            started_at,
            ended_at: Utc::now().timestamp(),
        };

        let config_file = match self.prepare(plan).await {
            Ok(path) => path,
            Err(err) => {
                log.error(format!("Could not prepare backup: {err}")).await;
                return BackupResult::Failed(stats(String::new()));
            }
        };

        let mut transferred = String::new();
        let mut result = None;
        'levels: for level in &plan.levels {
            let mut commands = Vec::new();
            if plan.policy.must_sync(*level) {
                commands.push("sync".to_owned());
            }
            commands.push(level.as_str().to_owned());

            for (index, command) in commands.iter().enumerate() {
                match self
                    .run_command(plan, &config_file, command, index + 1, log, cancel)
                    .await
                {
                    CommandResult::Ok(Some(s)) => transferred = s.transferred,
                    CommandResult::Ok(None) => {}
                    CommandResult::Failed => {
                        result = Some(BackupResult::Failed(stats(transferred.clone())));
                        break 'levels;
                    }
                    CommandResult::Cancelled => {
                        log.warn("Backup aborted").await;
                        result = Some(BackupResult::Cancelled);
                        break 'levels;
                    }
                }
            }
        }

        if let Err(err) = tokio::fs::remove_file(&config_file).await {
            log.warn(format!("Could not remove {}: {err}", config_file.display()))
                .await;
        }
        result.unwrap_or_else(|| BackupResult::Succeeded(stats(transferred)))
    }

    /// Writes the rsnapshot config and creates the snapshot root.
    async fn prepare(&self, plan: &BackupPlan<'_>) -> EngineResult<PathBuf> {
        for dir in [&self.config.tmp_dir, &self.config.log_dir] {
            tokio::fs::create_dir_all(dir)
                .await
                .map_err(EngineError::io(format!("creating {}", dir.display())))?;
        }
        let root = plan.snapshot_root();
        tokio::fs::create_dir_all(&root)
            .await
            .map_err(EngineError::io(format!("creating snapshot root {}", root.display())))?;

        let config = plan.rsnapshot_config(self.config);
        let path = self.config.tmp_dir.join(format!("rsnapshot.{}.cfg", plan.file_stem()));
        tokio::fs::write(&path, config.render())
            .await
            .map_err(EngineError::io(format!("writing {}", path.display())))?;
        Ok(path)
    }

    async fn run_command(
        &self,
        plan: &BackupPlan<'_>,
        config_file: &Path,
        command: &str,
        index: usize,
        log: &mut SubjectLog,
        cancel: &CancellationToken,
    ) -> CommandResult {
        let tmp_log = plan.tmp_log(self.config);
        let _ = tokio::fs::remove_file(&tmp_log).await;
        let spec = CommandSpec::new(&self.config.rsnapshot)
            .arg("-c")
            .arg(config_file.to_string_lossy())
            .arg(command);
        log.info(format!("Running {}", spec.display())).await;

        let outcome = match self.runner.run(&spec, cancel).await {
            Ok(outcome) => outcome,
            Err(err) => {
                log.error(format!("Could not start {}: {err}", self.config.rsnapshot))
                    .await;
                return CommandResult::Failed;
            }
        };
        // rsnapshot writes rsync's --stats to its log file; fall back to
        // the captured output when the log file was not written.
        let log_text = tokio::fs::read_to_string(&tmp_log)
            .await
            .unwrap_or_else(|_| outcome.output().to_owned());

        match outcome {
            Outcome::Cancelled => CommandResult::Cancelled,
            Outcome::Finished { success: true, .. } => {
                let stats = TransferStats::parse(&log_text);
                let summary = stats.as_ref().map(TransferStats::summary).unwrap_or_default();
                log.info(format!("Command succeeded. {summary}")).await;
                let _ = tokio::fs::remove_file(&tmp_log).await;
                CommandResult::Ok(stats)
            }
            Outcome::Finished { .. } => {
                let summary = summarize_failure(&log_text);
                let mut record = NewLogRecord::new(
                    LogLevel::Error,
                    crate::STATUS_SOURCE,
                    format!("Command failed:{}", excerpt(&summary)),
                )
                .about(log.subject());
                if let Some(file) = self.keep_failed_log(plan, &tmp_log, &log_text, index).await {
                    record = record.with_log_file(file);
                }
                log.push(record).await;
                CommandResult::Failed
            }
        }
    }

    /// Moves (or writes) the log of a failed run under `log_dir/jobs` and
    /// returns its file name.
    async fn keep_failed_log(&self, plan: &BackupPlan<'_>, tmp_log: &Path, text: &str, index: usize) -> Option<String> {
        let dir = self.config.job_log_dir();
        tokio::fs::create_dir_all(&dir).await.ok()?;
        let name = format!(
            "c{}j{}_{}_{index}.log",
            plan.client.id,
            plan.job.id,
            Utc::now().format("%Y%m%d%H%M%S")
        );
        let target = dir.join(&name);
        if tokio::fs::rename(tmp_log, &target).await.is_err() {
            tokio::fs::write(&target, text).await.ok()?;
        }
        Some(name)
    }
}

enum CommandResult {
    Ok(Option<TransferStats>),
    Failed,
    Cancelled,
}

/// Size of a directory tree in KiB, via `du -sk`.
pub async fn disk_usage_kb(runner: &dyn ProcessRunner, path: &Path) -> Option<i64> {
    let spec = CommandSpec::new("du").arg("-sk").arg(path.to_string_lossy());
    let outcome = runner.run(&spec, &CancellationToken::new()).await.ok()?;
    if !outcome.is_success() {
        return None;
    }
    outcome.output().split_whitespace().next()?.parse().ok()
}
