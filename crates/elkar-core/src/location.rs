//! Backup locations: where snapshots are stored.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{ValidationErrors, has_line_break};
use crate::ids::{ClientId, JobId, LocationId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocationSpec {
    pub name: String,
    /// NFS host; when set the directory is reached through the `/net` automounter.
    #[serde(default)]
    pub host: Option<String>,
    pub directory: String,
    #[serde(default = "default_parallel")]
    pub max_parallel_jobs: u32,
}

fn default_parallel() -> u32 {
    1
}

impl LocationSpec {
    pub fn validate(mut self) -> Result<Self, ValidationErrors> {
        let mut errors = ValidationErrors::new();
        self.name = self.name.trim().to_owned();
        self.directory = self.directory.trim().trim_end_matches('/').to_owned();
        self.host = self.host.map(|h| h.trim().to_owned()).filter(|h| !h.is_empty());

        errors.check(!self.name.is_empty(), "name", "name is required");
        errors.check(
            self.directory.starts_with('/') && !has_line_break(&self.directory),
            "directory",
            "directory must be an absolute path",
        );
        errors.check(
            self.host
                .as_deref()
                .is_none_or(|h| h.chars().all(|c| c.is_ascii_alphanumeric() || ".-_".contains(c))),
            "host",
            "host must be a plain host name",
        );
        errors.check(self.max_parallel_jobs >= 1, "max_parallel_jobs", "must be at least 1");
        errors.into_result().map(|()| self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupLocation {
    pub id: LocationId,
    #[serde(flatten)]
    pub spec: LocationSpec,
}

impl BackupLocation {
    /// Directory as seen from this machine.
    pub fn effective_dir(&self) -> PathBuf {
        match &self.spec.host {
            Some(host) => PathBuf::from(format!("/net/{host}{}", self.spec.directory)),
            None => PathBuf::from(&self.spec.directory),
        }
    }

    /// `<dir>/<client:04>/<job:04>`: rsnapshot's `snapshot_root` for a job.
    pub fn snapshot_root(&self, client: ClientId, job: JobId) -> PathBuf {
        snapshot_root(&self.effective_dir(), client, job)
    }
}

pub fn snapshot_root(base: &Path, client: ClientId, job: JobId) -> PathBuf {
    base.join(client.padded()).join(job.padded())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(host: Option<&str>, dir: &str) -> LocationSpec {
        LocationSpec {
            name: "Default".into(),
            host: host.map(Into::into),
            directory: dir.into(),
            max_parallel_jobs: 1,
        }
    }

    #[test]
    fn effective_dir_uses_net_for_remote_hosts() {
        let local = BackupLocation {
            id: LocationId(1),
            spec: spec(None, "/backups").validate().unwrap(),
        };
        let remote = BackupLocation {
            id: LocationId(2),
            spec: spec(Some("nas"), "/backups/").validate().unwrap(),
        };
        assert_eq!(local.effective_dir(), PathBuf::from("/backups"));
        assert_eq!(remote.effective_dir(), PathBuf::from("/net/nas/backups"));
        assert_eq!(
            local.snapshot_root(ClientId(1), JobId(23)),
            PathBuf::from("/backups/0001/0023")
        );
    }

    #[test]
    fn rejects_relative_directory_and_bad_host() {
        assert!(spec(None, "backups").validate().is_err());
        assert!(spec(Some("a/b"), "/x").validate().is_err());
        assert_eq!(spec(Some(""), "/x").validate().unwrap().host, None);
    }
}
