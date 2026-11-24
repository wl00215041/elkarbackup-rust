//! The SSH key pair the server uses to reach clients.

use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

use crate::process::{CommandSpec, Outcome, ProcessRunner};
use crate::{EngineError, EngineResult};

pub fn public_key_path(private_key: &Path) -> PathBuf {
    let mut name = private_key.as_os_str().to_owned();
    name.push(".pub");
    PathBuf::from(name)
}

/// The public key in `authorized_keys` format, if a key pair exists.
pub async fn read_public_key(private_key: &Path) -> Option<String> {
    tokio::fs::read_to_string(public_key_path(private_key)).await.ok()
}

/// Creates a new ed25519 key pair, replacing any existing one.
pub async fn generate(runner: &dyn ProcessRunner, private_key: &Path) -> EngineResult<String> {
    if let Some(dir) = private_key.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(EngineError::io(format!("creating {}", dir.display())))?;
    }
    for path in [private_key.to_path_buf(), public_key_path(private_key)] {
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(EngineError::io(format!("removing {}", path.display()))(e)),
        }
    }
    let spec = CommandSpec::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "elkarbackup", "-f"])
        .arg(private_key.to_string_lossy());
    match runner.run(&spec, &CancellationToken::new()).await {
        Ok(Outcome::Finished { success: true, .. }) => read_public_key(private_key)
            .await
            .ok_or_else(|| EngineError::Command("ssh-keygen produced no public key".into())),
        Ok(outcome) => Err(EngineError::Command(format!("ssh-keygen failed: {}", outcome.output()))),
        Err(err) => Err(EngineError::io("running ssh-keygen")(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SystemRunner;

    #[tokio::test]
    async fn generates_key_pair() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("ssh/id_ed25519");
        let public = generate(&SystemRunner, &key).await.unwrap();
        assert!(public.starts_with("ssh-ed25519 "));
        // Regenerating replaces the old pair.
        let second = generate(&SystemRunner, &key).await.unwrap();
        assert_ne!(public, second);
    }
}
