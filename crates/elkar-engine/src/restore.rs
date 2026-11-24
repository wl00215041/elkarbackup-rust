//! Copying files from a snapshot back to a client.

use std::path::{Path, PathBuf};

use elkar_core::client::Client;
use elkar_core::log::{LogLevel, NewLogRecord, Subject};
use elkar_core::rsnapshot::ssh_options;
use tokio_util::sync::CancellationToken;

use crate::Engine;
use crate::process::{CommandSpec, Outcome};
use crate::scripts::excerpt;

const SOURCE: &str = "Restore";

#[derive(Debug, Clone)]
pub struct RestoreRequest {
    /// Absolute, already validated path inside a snapshot.
    pub source: PathBuf,
    pub target: Client,
    /// Destination directory on the target client.
    pub target_path: String,
    pub requested_by: String,
}

impl RestoreRequest {
    fn command(&self, identity_file: &Path) -> CommandSpec {
        let ssh = format!("ssh {}", ssh_options(identity_file, &self.target.spec.ssh_args));
        let destination = self.target.source_url(&self.target_path);
        CommandSpec::new("rsync")
            .args(["-azh", "-e"])
            .arg(ssh)
            .arg(self.source.to_string_lossy())
            .arg(destination)
    }
}

impl Engine {
    /// Runs a restore in the background, reporting through the activity log.
    pub fn spawn_restore(&self, request: RestoreRequest) {
        let engine = self.clone();
        tokio::spawn(async move { engine.restore(request).await });
    }

    async fn restore(&self, request: RestoreRequest) {
        let subject = Subject::Client(request.target.id);
        let record = |level, message: String| {
            NewLogRecord::new(level, SOURCE, message)
                .about(subject)
                .by_user(&request.requested_by)
        };
        let activity = self.activity();
        let command = request.command(&self.config.ssh_key);
        activity
            .record(record(
                LogLevel::Info,
                format!("Restore started: {}", command.display()),
            ))
            .await;

        let (level, message) = match self.runner.run(&command, &CancellationToken::new()).await {
            Ok(Outcome::Finished { success: true, .. }) => (LogLevel::Info, "Restore finished successfully".to_owned()),
            Ok(Outcome::Finished { output, .. }) => (LogLevel::Error, format!("Restore failed.{}", excerpt(&output))),
            Ok(Outcome::Cancelled) => (LogLevel::Warning, "Restore cancelled".to_owned()),
            Err(err) => (LogLevel::Error, format!("Could not start rsync: {err}")),
        };
        activity.record(record(level, message)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use elkar_core::client::{ClientSpec, ClientState};
    use elkar_core::ids::ClientId;

    #[test]
    fn builds_rsync_command_without_shell() {
        let request = RestoreRequest {
            source: "/backups/0001/0002/daily.0/etc".into(),
            target: Client {
                id: ClientId(1),
                state: ClientState::Idle,
                spec: ClientSpec {
                    url: "root@host".into(),
                    ssh_args: "-p 2222".into(),
                    ..serde_default_client()
                },
            },
            target_path: "/tmp/restore".into(),
            requested_by: "admin".into(),
        };
        let cmd = request.command(Path::new("/keys/id"));
        assert_eq!(cmd.program, "rsync");
        assert_eq!(
            cmd.args,
            vec![
                "-azh",
                "-e",
                "ssh -i /keys/id -o BatchMode=yes -o StrictHostKeyChecking=no -p 2222",
                "/backups/0001/0002/daily.0/etc",
                "root@host:/tmp/restore",
            ]
        );
    }

    fn serde_default_client() -> ClientSpec {
        ClientSpec {
            name: "c".into(),
            description: String::new(),
            url: String::new(),
            is_active: true,
            quota_kb: None,
            owner_id: None,
            ssh_args: String::new(),
            rsync_short_args: String::new(),
            rsync_long_args: String::new(),
            max_parallel_jobs: 1,
            pre_scripts: vec![],
            post_scripts: vec![],
        }
    }
}
