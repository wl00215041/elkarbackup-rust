//! The server configuration file (TOML) and its environment overrides.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use elkar_engine::EngineConfig;
use elkar_engine::notify::{SmtpSecurity, SmtpSettings};
use elkar_web::WebConfig;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub database_url: String,
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// Absolute URL users reach the web interface at (used in emails).
    #[serde(default = "default_public_url")]
    pub public_url: String,
    #[serde(default)]
    pub paths: Paths,
    #[serde(default)]
    pub backup: Backup,
    #[serde(default)]
    pub web: Web,
    pub mail: Option<Mail>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Paths {
    /// Base for every other path that is not set explicitly.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    pub tmp_dir: Option<PathBuf>,
    pub log_dir: Option<PathBuf>,
    pub scripts_dir: Option<PathBuf>,
    pub ssh_key: Option<PathBuf>,
    /// Directory of the backup location created on first start.
    pub default_backup_dir: Option<PathBuf>,
}

impl Default for Paths {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            tmp_dir: None,
            log_dir: None,
            scripts_dir: None,
            ssh_key: None,
            default_backup_dir: None,
        }
    }
}

impl Paths {
    fn or_data(&self, value: &Option<PathBuf>, name: &str) -> PathBuf {
        value.clone().unwrap_or_else(|| self.data_dir.join(name))
    }

    pub fn default_backup_dir(&self) -> PathBuf {
        self.or_data(&self.default_backup_dir, "backups")
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backup {
    #[serde(default = "default_rsnapshot")]
    pub rsnapshot: String,
    #[serde(default = "one")]
    pub max_parallel_jobs: u32,
    #[serde(default = "yes")]
    pub post_on_pre_fail: bool,
    #[serde(default = "yes")]
    pub disk_usage: bool,
    /// Days to keep activity records; 0 keeps them forever.
    #[serde(default = "default_log_age")]
    pub max_log_age_days: u64,
    /// Command placed before user scripts, e.g. `["sudo", "-n"]`.
    #[serde(default)]
    pub script_prefix: Vec<String>,
}

impl Default for Backup {
    fn default() -> Self {
        Self {
            rsnapshot: default_rsnapshot(),
            max_parallel_jobs: 1,
            post_on_pre_fail: true,
            disk_usage: true,
            max_log_age_days: default_log_age(),
            script_prefix: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Web {
    #[serde(default)]
    pub secure_cookies: bool,
    #[serde(default = "default_session_hours")]
    pub session_hours: u64,
}

impl Default for Web {
    fn default() -> Self {
        Self {
            secure_cookies: false,
            session_hours: default_session_hours(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mail {
    pub host: String,
    #[serde(default = "default_smtp_port")]
    pub port: u16,
    #[serde(default = "default_security")]
    pub security: SmtpSecurity,
    pub username: Option<String>,
    pub password: Option<String>,
    pub from: String,
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8080))
}
fn default_public_url() -> String {
    "http://localhost:8080".into()
}
fn default_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/elkarbackup")
}
fn default_rsnapshot() -> String {
    "rsnapshot".into()
}
fn default_log_age() -> u64 {
    365
}
fn default_session_hours() -> u64 {
    8
}
fn default_smtp_port() -> u16 {
    587
}
fn default_security() -> SmtpSecurity {
    SmtpSecurity::Starttls
}
fn one() -> u32 {
    1
}
fn yes() -> bool {
    true
}

impl Config {
    /// Reads `path` and applies `DATABASE_URL` / `ELKARBACKUP_LISTEN`.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut config: Self = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if let Ok(url) = std::env::var("DATABASE_URL") {
            config.database_url = url;
        }
        if let Ok(listen) = std::env::var("ELKARBACKUP_LISTEN") {
            config.listen = listen.parse().context("ELKARBACKUP_LISTEN")?;
        }
        Ok(config)
    }

    pub fn engine(&self) -> EngineConfig {
        let paths = &self.paths;
        EngineConfig {
            tmp_dir: paths.or_data(&paths.tmp_dir, "tmp"),
            log_dir: paths.or_data(&paths.log_dir, "log"),
            scripts_dir: paths.or_data(&paths.scripts_dir, "scripts"),
            rsnapshot: self.backup.rsnapshot.clone(),
            tools: Default::default(),
            script_prefix: self.backup.script_prefix.clone(),
            max_parallel_jobs: self.backup.max_parallel_jobs.max(1),
            post_on_pre_fail: self.backup.post_on_pre_fail,
            disk_usage_enabled: self.backup.disk_usage,
            max_log_age: (self.backup.max_log_age_days > 0)
                .then(|| Duration::from_secs(self.backup.max_log_age_days * 86_400)),
            public_url: self.public_url.clone(),
            ssh_key: paths.or_data(&paths.ssh_key, "ssh/id_ed25519"),
        }
    }

    pub fn web(&self) -> WebConfig {
        WebConfig {
            secure_cookies: self.web.secure_cookies,
            session_ttl: Duration::from_secs(self.web.session_hours.max(1) * 3600),
        }
    }

    pub fn smtp(&self) -> Option<SmtpSettings> {
        self.mail.as_ref().map(|m| SmtpSettings {
            host: m.host.clone(),
            port: m.port,
            security: m.security,
            username: m.username.clone(),
            password: m.password.clone(),
            from: m.from.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let text = include_str!("../../../config/elkarbackup.example.toml");
        let config: Config = toml::from_str(text).unwrap();
        let engine = config.engine();
        assert_eq!(engine.scripts_dir, PathBuf::from("/var/lib/elkarbackup/scripts"));
        assert_eq!(engine.max_log_age, Some(Duration::from_secs(365 * 86_400)));
        assert!(config.smtp().is_none());
    }

    #[test]
    fn minimal_config_uses_defaults() {
        let config: Config = toml::from_str("database_url = \"postgres://x\"").unwrap();
        assert_eq!(config.listen, default_listen());
        assert_eq!(
            config.engine().ssh_key,
            PathBuf::from("/var/lib/elkarbackup/ssh/id_ed25519")
        );
        assert!(toml::from_str::<Config>("database_url = \"x\"\nunknown = 1").is_err());
    }
}
