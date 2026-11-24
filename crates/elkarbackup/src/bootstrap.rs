//! Records a fresh installation needs before it is usable.

use anyhow::Context;
use chrono::Weekday;
use elkar_core::location::LocationSpec;
use elkar_core::policy::{PolicySpec, RetainLevel, RetainRule};
use elkar_core::schedule::{Schedule, parse};
use elkar_core::user::{Role, UserSpec, validate_password};
use elkar_storage::Store;

use crate::config::Config;

/// Creates a default backup location and policy when none exist.
pub async fn ensure_defaults(store: &Store, config: &Config) -> anyhow::Result<()> {
    if store.locations().list().await?.is_empty() {
        let directory = config.paths.default_backup_dir();
        store
            .locations()
            .create(
                &LocationSpec {
                    name: "Default".into(),
                    host: None,
                    directory: directory.display().to_string(),
                    max_parallel_jobs: 1,
                }
                .validate()?,
            )
            .await?;
        tracing::info!(directory = %directory.display(), "created default backup location");
    }
    if store.policies().list().await?.is_empty() {
        store.policies().create(&default_policy().validate()?).await?;
        tracing::info!("created default policy");
    }
    if store.users().count().await? == 0 {
        tracing::warn!("no users exist yet; create one with `elkarbackup create-admin`");
    }
    Ok(())
}

/// Seven daily and four weekly snapshots taken at 21:00.
fn default_policy() -> PolicySpec {
    let at_nine = || parse::times("21:00").expect("valid literal");
    PolicySpec {
        name: "Default policy".into(),
        description: "Daily backups kept for a week, weekly backups kept for a month".into(),
        include: String::new(),
        exclude: String::new(),
        sync_first: false,
        retains: vec![
            RetainRule {
                level: RetainLevel::Daily,
                count: 7,
                schedule: Schedule {
                    times: at_nine(),
                    ..Schedule::default()
                },
            },
            RetainRule {
                level: RetainLevel::Weekly,
                count: 4,
                schedule: Schedule {
                    times: at_nine(),
                    days_of_week: vec![Weekday::Sun],
                    ..Schedule::default()
                },
            },
        ],
    }
}

pub async fn create_admin(store: &Store, username: &str, email: &str, password: &str) -> anyhow::Result<()> {
    validate_password(password)?;
    let spec = UserSpec {
        username: username.into(),
        email: email.into(),
        role: Role::Admin,
        is_active: true,
        lines_per_page: 20,
    }
    .validate()?;
    let hash = elkar_web::hash_password(password).map_err(anyhow::Error::msg)?;
    store
        .users()
        .create(&spec, &hash)
        .await
        .context("creating the administrator")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_valid() {
        let policy = default_policy().validate().unwrap();
        assert_eq!(policy.retains.len(), 2);
    }
}
