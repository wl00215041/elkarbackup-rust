//! Rendering of rsnapshot configuration files.
//!
//! rsnapshot requires TAB separated `key<TAB>value` lines; every value
//! placed here comes from validated specs that forbid line breaks.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::policy::RetainLevel;

/// Paths of the external binaries rsnapshot drives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tools {
    pub cp: String,
    pub rm: String,
    pub rsync: String,
    pub ssh: String,
}

impl Default for Tools {
    fn default() -> Self {
        Self {
            cp: "/bin/cp".into(),
            rm: "/bin/rm".into(),
            rsync: "/usr/bin/rsync".into(),
            ssh: "/usr/bin/ssh".into(),
        }
    }
}

/// Non interactive ssh options using `identity_file`, followed by `extra`.
pub fn ssh_options(identity_file: &Path, extra: &str) -> String {
    let options = format!(
        "-i {} -o BatchMode=yes -o StrictHostKeyChecking=no {extra}",
        identity_file.display()
    );
    options.trim_end().to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RsnapshotConfig {
    pub tools: Tools,
    pub snapshot_root: PathBuf,
    pub retains: Vec<(RetainLevel, u32)>,
    pub log_file: PathBuf,
    pub lock_file: PathBuf,
    pub rsync_short_args: String,
    pub rsync_long_args: String,
    pub use_local_permissions: bool,
    /// Private key offered to clients.
    pub identity_file: PathBuf,
    /// Extra ssh arguments configured on the client.
    pub ssh_args: String,
    pub includes: Vec<String>,
    pub excludes: Vec<String>,
    pub sync_first: bool,
    pub source: String,
}

impl RsnapshotConfig {
    pub fn render(&self) -> String {
        let mut out = String::new();
        let mut line = |key: &str, value: &dyn std::fmt::Display| {
            let _ = writeln!(out, "{key}\t{value}");
        };

        line("config_version", &"1.2");
        line("snapshot_root", &self.snapshot_root.display());
        line("cmd_cp", &self.tools.cp);
        line("cmd_rm", &self.tools.rm);
        line("cmd_rsync", &self.tools.rsync);
        line("cmd_ssh", &self.tools.ssh);
        for (level, count) in &self.retains {
            line("retain", &format_args!("{level}\t{count}"));
        }
        line("verbose", &2);
        line("loglevel", &4);
        line("logfile", &self.log_file.display());
        line("lockfile", &self.lock_file.display());
        if !self.rsync_short_args.is_empty() {
            line("rsync_short_args", &format_args!("-a{}", self.rsync_short_args));
        }
        let permissions = if self.use_local_permissions {
            " --chmod=Fug=rwX,Dug=rwx,Fo=r,Do=rx"
        } else {
            ""
        };
        line(
            "rsync_long_args",
            &format_args!(
                "--delete --numeric-ids --relative --delete-excluded{permissions} --stats {}",
                self.rsync_long_args
            ),
        );
        line("ssh_args", &ssh_options(&self.identity_file, &self.ssh_args));
        for include in &self.includes {
            line("include", include);
        }
        for exclude in &self.excludes {
            line("exclude", exclude);
        }
        line("sync_first", &u8::from(self.sync_first));
        line("backup", &format_args!("{}\t.", self.source));
        out
    }
}

/// Splits newline separated rsync patterns. rsnapshot cannot quote spaces,
/// so escaped spaces become the `?` single character wildcard.
pub fn split_patterns(patterns: &str) -> Vec<String> {
    patterns
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| l.replace("\\ ", "?"))
        .collect()
}

/// The rsync `--stats` summary lines worth keeping in the activity log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransferStats {
    pub files_total: String,
    pub files_created: String,
    pub files_deleted: String,
    pub transferred: String,
}

impl TransferStats {
    /// Extracts the summary from rsnapshot/rsync output; `None` if absent.
    pub fn parse(output: &str) -> Option<Self> {
        let field = |label: &str| {
            output.lines().find_map(|line| {
                let line = strip_timestamp(line);
                line.strip_prefix(label)
                    .and_then(|rest| rest.strip_prefix(':'))
                    .map(|v| v.trim().to_owned())
            })
        };
        Some(Self {
            files_total: field("Number of files")?,
            files_created: field("Number of created files")?,
            files_deleted: field("Number of deleted files")?,
            transferred: field("Total transferred file size")?,
        })
    }

    pub fn summary(&self) -> String {
        format!(
            "Number of files: {}\nNumber of created files: {}\nNumber of deleted files: {}\nTotal transferred file size: {}",
            self.files_total, self.files_created, self.files_deleted, self.transferred
        )
    }
}

/// rsnapshot log lines start with `[timestamp] `; drop it when present.
fn strip_timestamp(line: &str) -> &str {
    match line.strip_prefix('[') {
        Some(rest) => rest.split_once("] ").map_or(line, |(_, msg)| msg),
        None => line,
    }
}

/// Picks the most informative lines of a failed rsnapshot log: well known
/// error lines, else the last five lines.
pub fn summarize_failure(log: &str) -> String {
    const MARKERS: [&str; 4] = ["ssh:", "permission", "such file", "sigterm"];
    let lines: Vec<&str> = log.lines().map(strip_timestamp).collect();
    let known: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| {
            let lower = l.to_lowercase();
            MARKERS.iter().any(|m| lower.contains(m))
        })
        .collect();
    if known.is_empty() {
        let tail = &lines[lines.len().saturating_sub(5)..];
        format!("[unknown error] {}", tail.join("\n"))
    } else {
        known.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> RsnapshotConfig {
        RsnapshotConfig {
            tools: Tools::default(),
            snapshot_root: "/backups/0001/0002".into(),
            retains: vec![(RetainLevel::Daily, 7), (RetainLevel::Weekly, 4)],
            log_file: "/var/log/elkar/tmp-c0001j0002.log".into(),
            lock_file: "/tmp/rsnapshot.0001_0002.pid".into(),
            rsync_short_args: "z".into(),
            rsync_long_args: "--bwlimit=100".into(),
            use_local_permissions: false,
            identity_file: "/var/lib/elkar/ssh/id_ed25519".into(),
            ssh_args: "-p 2222".into(),
            includes: vec!["/etc".into()],
            excludes: vec!["*.tmp".into()],
            sync_first: true,
            source: "root@host:/etc".into(),
        }
    }

    #[test]
    fn renders_tab_separated_config() {
        let text = config().render();
        assert!(text.contains("snapshot_root\t/backups/0001/0002\n"));
        assert!(text.contains("retain\tdaily\t7\nretain\tweekly\t4\n"));
        assert!(text.contains("rsync_short_args\t-az\n"));
        assert!(
            text.contains(
                "rsync_long_args\t--delete --numeric-ids --relative --delete-excluded --stats --bwlimit=100\n"
            )
        );
        assert!(text.contains(
            "ssh_args\t-i /var/lib/elkar/ssh/id_ed25519 -o BatchMode=yes -o StrictHostKeyChecking=no -p 2222\n"
        ));
        assert!(text.contains("include\t/etc\nexclude\t*.tmp\n"));
        assert!(text.contains("sync_first\t1\n"));
        assert!(text.ends_with("backup\troot@host:/etc\t.\n"));
    }

    #[test]
    fn local_permissions_add_chmod() {
        let mut c = config();
        c.use_local_permissions = true;
        c.rsync_short_args.clear();
        let text = c.render();
        assert!(text.contains("--chmod=Fug=rwX,Dug=rwx,Fo=r,Do=rx --stats"));
        assert!(!text.contains("rsync_short_args"));
    }

    #[test]
    fn patterns_escape_spaces() {
        assert_eq!(split_patterns(" a\\ b \n\n c\r\n"), vec!["a?b", "c"]);
    }

    #[test]
    fn parses_rsync_stats() {
        let log = "[2024-01-01T00:00:00] Number of files: 10 (reg: 9, dir: 1)\n\
                   [2024-01-01T00:00:00] Number of created files: 2\n\
                   [2024-01-01T00:00:00] Number of deleted files: 0\n\
                   [2024-01-01T00:00:00] Total transferred file size: 1,024 bytes\n";
        let stats = TransferStats::parse(log).unwrap();
        assert_eq!(stats.files_total, "10 (reg: 9, dir: 1)");
        assert_eq!(stats.transferred, "1,024 bytes");
        assert!(TransferStats::parse("nothing").is_none());
    }

    #[test]
    fn failure_summary_prefers_known_errors() {
        let log = "[t] a\n[t] ssh: connect to host x port 22: Connection refused\n[t] b\n";
        assert_eq!(
            summarize_failure(log),
            "ssh: connect to host x port 22: Connection refused"
        );
        let unknown = summarize_failure("1\n2\n3\n4\n5\n6\n");
        assert_eq!(unknown, "[unknown error] 2\n3\n4\n5\n6");
    }
}
