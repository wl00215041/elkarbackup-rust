//! Running external programs with cancellation support.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Upper bound on captured output kept in memory per stream.
const MAX_CAPTURE: usize = 256 * 1024;
/// How long a cancelled process gets to exit after SIGTERM before SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(10);

/// A program invocation. Arguments are passed directly, never through a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
}

impl CommandSpec {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl ToString) -> Self {
        self.env.insert(key.into(), value.to_string());
        self
    }

    /// Wraps the command with a prefix such as `sudo`.
    pub fn prefixed(self, prefix: &[String]) -> Self {
        match prefix.split_first() {
            None => self,
            Some((program, rest)) => {
                let mut args: Vec<String> = rest.to_vec();
                args.push(self.program);
                args.extend(self.args);
                Self {
                    program: program.clone(),
                    args,
                    env: self.env,
                    cwd: self.cwd,
                }
            }
        }
    }

    /// Human readable form for logs.
    pub fn display(&self) -> String {
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The program ran to completion (successfully or not).
    Finished {
        success: bool,
        code: Option<i32>,
        output: String,
    },
    /// The cancellation token fired and the program was stopped.
    Cancelled,
}

impl Outcome {
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Finished { success: true, .. })
    }

    pub fn output(&self) -> &str {
        match self {
            Self::Finished { output, .. } => output,
            Self::Cancelled => "",
        }
    }
}

#[async_trait]
pub trait ProcessRunner: Send + Sync {
    /// Runs `spec`; stdout and stderr are captured together.
    async fn run(&self, spec: &CommandSpec, cancel: &CancellationToken) -> std::io::Result<Outcome>;
}

/// Runs real processes with `tokio::process`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRunner;

#[async_trait]
impl ProcessRunner for SystemRunner {
    async fn run(&self, spec: &CommandSpec, cancel: &CancellationToken) -> std::io::Result<Outcome> {
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .envs(&spec.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        let mut child = command.spawn()?;
        let stdout = tokio::spawn(capture(child.stdout.take()));
        let stderr = tokio::spawn(capture(child.stderr.take()));

        let status = tokio::select! {
            status = child.wait() => status?,
            () = cancel.cancelled() => {
                terminate(&mut child).await;
                return Ok(Outcome::Cancelled);
            }
        };

        let mut output = stdout.await.unwrap_or_default();
        let err = stderr.await.unwrap_or_default();
        if !err.is_empty() {
            if !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(&err);
        }
        Ok(Outcome::Finished {
            success: status.success(),
            code: status.code(),
            output,
        })
    }
}

async fn capture<R: AsyncRead + Unpin>(stream: Option<R>) -> String {
    let Some(mut stream) = stream else {
        return String::new();
    };
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 8192];
    while let Ok(n) = stream.read(&mut chunk).await {
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_CAPTURE {
            // Keep the tail: the end of a log is the most useful part.
            buf.drain(..buf.len() - MAX_CAPTURE);
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// SIGTERM first so rsnapshot can clean up its lock file, then SIGKILL.
async fn terminate(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id().and_then(|id| i32::try_from(id).ok()) {
        let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
        if tokio::time::timeout(TERM_GRACE, child.wait()).await.is_ok() {
            return;
        }
    }
    let _ = child.kill().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_wraps_program() {
        let spec = CommandSpec::new("/s/0001.script")
            .arg("x")
            .prefixed(&["sudo".into(), "-n".into()]);
        assert_eq!(spec.program, "sudo");
        assert_eq!(spec.args, vec!["-n", "/s/0001.script", "x"]);
    }

    #[tokio::test]
    async fn captures_both_streams_and_status() {
        let spec = CommandSpec::new("sh").args(["-c", "echo out; echo err >&2; exit 3"]);
        let outcome = SystemRunner.run(&spec, &CancellationToken::new()).await.unwrap();
        match outcome {
            Outcome::Finished { success, code, output } => {
                assert!(!success);
                assert_eq!(code, Some(3));
                assert_eq!(output, "out\nerr\n");
            }
            Outcome::Cancelled => panic!("not cancelled"),
        }
    }

    #[tokio::test]
    async fn cancellation_stops_process() {
        let cancel = CancellationToken::new();
        let spec = CommandSpec::new("sleep").arg("30");
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            trigger.cancel();
        });
        let started = std::time::Instant::now();
        let outcome = SystemRunner.run(&spec, &cancel).await.unwrap();
        assert_eq!(outcome, Outcome::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
