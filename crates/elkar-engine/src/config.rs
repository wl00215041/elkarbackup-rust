use std::path::PathBuf;
use std::time::Duration;

use elkar_core::rsnapshot::Tools;

/// Engine settings; built by the binary from its configuration file.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Working directory for generated rsnapshot configs and lock files.
    pub tmp_dir: PathBuf,
    /// Directory receiving rsnapshot logs; failed runs are kept in `jobs/`.
    pub log_dir: PathBuf,
    /// Directory holding uploaded scripts (`0001.script`).
    pub scripts_dir: PathBuf,
    pub rsnapshot: String,
    pub tools: Tools,
    /// Program and arguments placed before every user script (e.g. `["sudo"]`).
    pub script_prefix: Vec<String>,
    /// Global limit of jobs past the `queued` state.
    pub max_parallel_jobs: u32,
    /// Run post job scripts even when pre job scripts fail.
    pub post_on_pre_fail: bool,
    pub disk_usage_enabled: bool,
    /// Log records older than this are purged; `None` keeps them forever.
    pub max_log_age: Option<Duration>,
    /// Absolute URL of the web interface, used in notification links.
    pub public_url: String,
    /// SSH private key used for client connections; its `.pub` is published.
    pub ssh_key: PathBuf,
}

impl EngineConfig {
    /// Defaults rooted at `base`, convenient for tests and development.
    pub fn rooted_at(base: impl Into<PathBuf>) -> Self {
        let base = base.into();
        Self {
            tmp_dir: base.join("tmp"),
            log_dir: base.join("log"),
            scripts_dir: base.join("scripts"),
            rsnapshot: "rsnapshot".into(),
            tools: Tools::default(),
            script_prefix: Vec::new(),
            max_parallel_jobs: 1,
            post_on_pre_fail: true,
            disk_usage_enabled: true,
            max_log_age: None,
            public_url: "http://localhost:8080".into(),
            ssh_key: base.join("ssh").join("id_ed25519"),
        }
    }

    pub fn job_log_dir(&self) -> PathBuf {
        self.log_dir.join("jobs")
    }
}
