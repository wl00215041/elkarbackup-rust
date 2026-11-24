//! Safe browsing of snapshot directories.

use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::{EngineError, EngineResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub modified: Option<DateTime<Utc>>,
}

/// Resolves `relative` inside `root`, rejecting anything (including
/// symlinks) that ends up outside of it.
pub async fn resolve(root: &Path, relative: &str) -> EngineResult<PathBuf> {
    let relative = Path::new(relative.trim_start_matches('/'));
    if relative
        .components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(EngineError::InvalidPath);
    }
    let root = tokio::fs::canonicalize(root)
        .await
        .map_err(|_| EngineError::NotFound("snapshot root"))?;
    let resolved = tokio::fs::canonicalize(root.join(relative))
        .await
        .map_err(|_| EngineError::NotFound("path"))?;
    if resolved.starts_with(&root) {
        Ok(resolved)
    } else {
        Err(EngineError::InvalidPath)
    }
}

/// Directory contents, directories first, then by name.
pub async fn list_dir(path: &Path) -> EngineResult<Vec<DirEntry>> {
    let mut reader = tokio::fs::read_dir(path)
        .await
        .map_err(EngineError::io(format!("reading {}", path.display())))?;
    let mut entries = Vec::new();
    while let Some(entry) = reader
        .next_entry()
        .await
        .map_err(EngineError::io(format!("reading {}", path.display())))?
    {
        let Ok(link_meta) = entry.metadata().await else {
            continue;
        };
        let is_symlink = link_meta.file_type().is_symlink();
        let meta = if is_symlink {
            tokio::fs::metadata(entry.path()).await.unwrap_or(link_meta)
        } else {
            link_meta
        };
        entries.push(DirEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            is_dir: meta.is_dir(),
            is_symlink,
            size: meta.len(),
            modified: meta.modified().ok().map(DateTime::<Utc>::from),
        });
    }
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
    Ok(entries)
}

/// Capacity of the filesystem holding `path`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsUsage {
    pub total_bytes: u64,
    pub used_bytes: u64,
}

impl FsUsage {
    pub fn used_percent(self) -> u64 {
        (self.used_bytes * 100).checked_div(self.total_bytes).unwrap_or(0)
    }
}

pub fn fs_usage(path: &Path) -> Option<FsUsage> {
    let stat = nix::sys::statvfs::statvfs(path).ok()?;
    let fragment = stat.fragment_size();
    let total_bytes = stat.blocks() * fragment;
    let free_bytes = stat.blocks_free() * fragment;
    Some(FsUsage {
        total_bytes,
        used_bytes: total_bytes.saturating_sub(free_bytes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolve_rejects_traversal_and_escaping_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("daily.0/etc")).unwrap();
        std::fs::write(root.join("daily.0/etc/hosts"), "x").unwrap();
        std::fs::write(dir.path().join("secret"), "s").unwrap();
        std::os::unix::fs::symlink(dir.path().join("secret"), root.join("daily.0/link")).unwrap();

        let ok = resolve(&root, "/daily.0/etc/hosts").await.unwrap();
        assert!(ok.ends_with("daily.0/etc/hosts"));
        assert!(matches!(
            resolve(&root, "../secret").await,
            Err(EngineError::InvalidPath)
        ));
        assert!(matches!(
            resolve(&root, "daily.0/link").await,
            Err(EngineError::InvalidPath)
        ));
        assert!(matches!(resolve(&root, "missing").await, Err(EngineError::NotFound(_))));

        let listing = list_dir(&root.join("daily.0")).await.unwrap();
        assert_eq!(listing[0].name, "etc");
        assert!(listing[0].is_dir);
        assert!(listing[1].is_symlink);
    }
}
