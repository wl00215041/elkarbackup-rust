//! Authorization and the rules shared by the HTML interface and the API.
//!
//! Administrators manage everything. Regular users see and manage only the
//! clients they own (and those clients' jobs, backups and logs).

use std::os::unix::fs::PermissionsExt;

use elkar_core::ValidationErrors;
use elkar_core::client::{Client, ClientSpec};
use elkar_core::ids::{ClientId, JobId, ScriptId, UserId};
use elkar_core::job::{Job, JobSpec};
use elkar_core::log::{LogLevel, NewLogRecord, Subject};
use elkar_core::script::{self, Level, Phase, ScriptSpec};
use elkar_core::user::{User, UserSpec, validate_password};
use elkar_engine::EnqueueOutcome;
use elkar_storage::{ClientFilter, LogFilter, Store};

use crate::AppState;
use crate::auth::{hash_password, random_token};
use crate::error::{AppError, AppResult};

const AUDIT_SOURCE: &str = "Web";

pub struct Access<'a> {
    state: &'a AppState,
    user: &'a User,
}

impl<'a> Access<'a> {
    pub fn new(state: &'a AppState, user: &'a User) -> Self {
        Self { state, user }
    }

    fn store(&self) -> &Store {
        self.state.store()
    }

    pub fn is_admin(&self) -> bool {
        self.user.is_admin()
    }

    pub fn require_admin(&self) -> AppResult<()> {
        if self.is_admin() {
            Ok(())
        } else {
            Err(AppError::Forbidden)
        }
    }

    /// Records an action taken by the current user.
    pub async fn audit(&self, message: impl Into<String>, subject: Option<Subject>) {
        let mut record = NewLogRecord::new(LogLevel::Info, AUDIT_SOURCE, message).by_user(&self.user.spec.username);
        record.subject = subject;
        self.state.engine.activity().record(record).await;
    }

    // ----- clients -------------------------------------------------------

    pub async fn clients(&self, name: Option<String>) -> AppResult<Vec<Client>> {
        let filter = ClientFilter {
            owner: (!self.is_admin()).then_some(self.user.id),
            name: name.filter(|n| !n.trim().is_empty()),
        };
        Ok(self.store().clients().list(&filter).await?)
    }

    pub async fn client(&self, id: ClientId) -> AppResult<Client> {
        let client = self.store().clients().get(id).await?;
        if self.is_admin() || client.is_owned_by(self.user.id) {
            Ok(client)
        } else {
            Err(AppError::Forbidden)
        }
    }

    /// Creates or updates a client. Ownership and the raw ssh/rsync
    /// arguments are admin only: those arguments end up on the command line
    /// of processes run by the server, so letting owners set them would let
    /// any user run commands on the backup server.
    pub async fn save_client(&self, id: Option<ClientId>, mut spec: ClientSpec) -> AppResult<ClientId> {
        if !self.is_admin() {
            let protected = match id {
                Some(id) => self.client(id).await?.spec,
                None => ClientSpec {
                    owner_id: Some(self.user.id),
                    ssh_args: String::new(),
                    rsync_short_args: String::new(),
                    rsync_long_args: String::new(),
                    ..spec.clone()
                },
            };
            spec.owner_id = protected.owner_id;
            spec.ssh_args = protected.ssh_args;
            spec.rsync_short_args = protected.rsync_short_args;
            spec.rsync_long_args = protected.rsync_long_args;
        } else if let Some(id) = id {
            self.client(id).await?;
        }
        let spec = spec.validate()?;
        self.check_scripts(&spec.pre_scripts, Level::Client, Phase::Pre, "pre_scripts")
            .await?;
        self.check_scripts(&spec.post_scripts, Level::Client, Phase::Post, "post_scripts")
            .await?;
        if let Some(owner) = spec.owner_id {
            self.store()
                .users()
                .get(owner)
                .await
                .map_err(|_| ValidationErrors::single("owner_id", "unknown user"))?;
        }

        let id = match id {
            Some(id) => {
                self.store().clients().update(id, &spec).await?;
                id
            }
            None => self.store().clients().create(&spec).await?,
        };
        self.audit(format!("Client \"{}\" saved", spec.name), Some(Subject::Client(id)))
            .await;
        Ok(id)
    }

    /// Deletes the client and its jobs. Snapshot data on disk is kept.
    pub async fn delete_client(&self, id: ClientId) -> AppResult<()> {
        let client = self.client(id).await?;
        self.store().clients().delete(id).await?;
        self.audit(
            format!("Client \"{}\" deleted", client.spec.name),
            Some(Subject::Client(id)),
        )
        .await;
        Ok(())
    }

    /// Copies a client and its jobs under a new name.
    pub async fn clone_client(&self, id: ClientId) -> AppResult<ClientId> {
        let client = self.client(id).await?;
        let mut spec = client.spec.clone();
        spec.name = self.unique_client_name(&client.spec.name).await?;
        let copy = self.store().clients().create(&spec).await?;
        for job in self.store().jobs().list_for_client(id).await? {
            self.store().jobs().create(copy, &job.spec).await?;
        }
        self.audit(
            format!("Client \"{}\" cloned from \"{}\"", spec.name, client.spec.name),
            Some(Subject::Client(copy)),
        )
        .await;
        Ok(copy)
    }

    async fn unique_client_name(&self, base: &str) -> AppResult<String> {
        let existing: Vec<String> = self
            .store()
            .clients()
            .list(&ClientFilter::default())
            .await?
            .into_iter()
            .map(|c| c.spec.name)
            .collect();
        Ok((1..)
            .map(|n| {
                if n == 1 {
                    format!("{base} (copy)")
                } else {
                    format!("{base} (copy {n})")
                }
            })
            .find(|name| !existing.contains(name))
            .expect("unbounded range"))
    }

    // ----- jobs ----------------------------------------------------------

    pub async fn job(&self, client_id: ClientId, job_id: JobId) -> AppResult<(Client, Job)> {
        let client = self.client(client_id).await?;
        let job = self.store().jobs().get(job_id).await?;
        if job.client_id != client.id {
            return Err(AppError::NotFound);
        }
        Ok((client, job))
    }

    /// Looks a job up by id alone (API), checking access to its client.
    pub async fn job_by_id(&self, job_id: JobId) -> AppResult<(Client, Job)> {
        let job = self.store().jobs().get(job_id).await?;
        let client = self.client(job.client_id).await?;
        Ok((client, job))
    }

    pub async fn jobs(&self) -> AppResult<Vec<Job>> {
        let jobs = self.store().jobs().list().await?;
        if self.is_admin() {
            return Ok(jobs);
        }
        let mine: Vec<ClientId> = self.clients(None).await?.into_iter().map(|c| c.id).collect();
        Ok(jobs.into_iter().filter(|j| mine.contains(&j.client_id)).collect())
    }

    pub async fn save_job(&self, client_id: ClientId, id: Option<JobId>, spec: JobSpec) -> AppResult<JobId> {
        if let Some(id) = id {
            self.job(client_id, id).await?;
        } else {
            self.client(client_id).await?;
        }
        let spec = spec.validate()?;
        let mut errors = ValidationErrors::new();
        errors.check(
            self.store().policies().get(spec.policy_id).await.is_ok(),
            "policy_id",
            "unknown policy",
        );
        errors.check(
            self.store().locations().get(spec.location_id).await.is_ok(),
            "location_id",
            "unknown backup location",
        );
        errors.into_result()?;
        self.check_scripts(&spec.pre_scripts, Level::Job, Phase::Pre, "pre_scripts")
            .await?;
        self.check_scripts(&spec.post_scripts, Level::Job, Phase::Post, "post_scripts")
            .await?;

        let id = match id {
            Some(id) => {
                self.store().jobs().update(id, &spec).await?;
                id
            }
            None => self.store().jobs().create(client_id, &spec).await?,
        };
        self.audit(
            format!("Job \"{}\" saved", spec.name),
            Some(Subject::Job(client_id, id)),
        )
        .await;
        Ok(id)
    }

    pub async fn delete_job(&self, client_id: ClientId, job_id: JobId) -> AppResult<()> {
        let (_, job) = self.job(client_id, job_id).await?;
        self.store().jobs().delete(job_id).await?;
        self.audit(
            format!("Job \"{}\" deleted", job.spec.name),
            Some(Subject::Job(client_id, job_id)),
        )
        .await;
        Ok(())
    }

    /// Creates a new anonymous trigger token, replacing any previous one.
    pub async fn regenerate_token(&self, client_id: ClientId, job_id: JobId) -> AppResult<String> {
        self.job(client_id, job_id).await?;
        let token = random_token();
        self.store().jobs().set_token(job_id, Some(&token)).await?;
        Ok(token)
    }

    pub async fn clear_token(&self, client_id: ClientId, job_id: JobId) -> AppResult<()> {
        self.job(client_id, job_id).await?;
        self.store().jobs().set_token(job_id, None).await?;
        Ok(())
    }

    pub async fn enqueue(&self, client_id: ClientId, job_id: JobId) -> AppResult<EnqueueOutcome> {
        self.job(client_id, job_id).await?;
        Ok(self
            .state
            .engine
            .enqueue_now(client_id, job_id, Some(&self.user.spec.username))
            .await?)
    }

    pub async fn abort(&self, client_id: ClientId, job_id: JobId) -> AppResult<bool> {
        self.job(client_id, job_id).await?;
        Ok(self
            .state
            .engine
            .request_abort(client_id, job_id, &self.user.spec.username)
            .await?)
    }

    // ----- scripts -------------------------------------------------------

    async fn check_scripts(&self, ids: &[ScriptId], level: Level, phase: Phase, field: &'static str) -> AppResult<()> {
        for id in ids {
            let allowed = self
                .store()
                .scripts()
                .get(*id)
                .await
                .is_ok_and(|s| s.spec.allows(level, phase));
            if !allowed {
                return Err(
                    ValidationErrors::single(field, format!("script {id} cannot run at {level} {phase}")).into(),
                );
            }
        }
        Ok(())
    }

    /// Creates or updates a script; `content` replaces the script file.
    pub async fn save_script(
        &self,
        id: Option<ScriptId>,
        spec: ScriptSpec,
        content: Option<Vec<u8>>,
    ) -> AppResult<ScriptId> {
        self.require_admin()?;
        let spec = spec.validate()?;
        if id.is_none() && content.as_ref().is_none_or(Vec::is_empty) {
            return Err(ValidationErrors::single("file", "a script file is required").into());
        }
        let id = match id {
            Some(id) => {
                self.store().scripts().update(id, &spec).await?;
                id
            }
            None => self.store().scripts().create(&spec).await?,
        };
        if let Some(content) = content.filter(|c| !c.is_empty()) {
            self.write_script_file(id, &content).await?;
        }
        self.audit(format!("Script \"{}\" saved", spec.name), None).await;
        Ok(id)
    }

    async fn write_script_file(&self, id: ScriptId, content: &[u8]) -> AppResult<()> {
        let dir = &self.state.engine.config.scripts_dir;
        tokio::fs::create_dir_all(dir).await.map_err(AppError::internal)?;
        let path = dir.join(script::file_name(id));
        tokio::fs::write(&path, content).await.map_err(AppError::internal)?;
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .await
            .map_err(AppError::internal)
    }

    pub async fn delete_script(&self, id: ScriptId) -> AppResult<()> {
        self.require_admin()?;
        let script = self.store().scripts().get(id).await?;
        self.store().scripts().delete(id).await?;
        let path = self.state.engine.config.scripts_dir.join(script.file_name());
        if let Err(err) = tokio::fs::remove_file(&path).await {
            tracing::warn!(error = %err, path = %path.display(), "could not remove script file");
        }
        self.audit(format!("Script \"{}\" deleted", script.spec.name), None)
            .await;
        Ok(())
    }

    // ----- users ---------------------------------------------------------

    pub async fn save_user(&self, id: Option<UserId>, spec: UserSpec, password: Option<String>) -> AppResult<UserId> {
        self.require_admin()?;
        let spec = spec.validate()?;
        let password = password.filter(|p| !p.is_empty());
        if id.is_none() && password.is_none() {
            return Err(ValidationErrors::single("password", "a password is required").into());
        }
        if let Some(password) = &password {
            validate_password(password)?;
        }
        if let Some(id) = id {
            let current = self.store().users().get(id).await?;
            let demotes_admin =
                current.is_admin() && (!spec.is_active || !matches!(spec.role, elkar_core::user::Role::Admin));
            if demotes_admin && self.store().users().count_active_admins().await? <= 1 {
                return Err(AppError::Conflict(
                    "at least one active administrator is required".into(),
                ));
            }
        }
        let hash = password
            .map(|p| hash_password(&p))
            .transpose()
            .map_err(AppError::Internal)?;
        let id = match id {
            Some(id) => {
                self.store().users().update(id, &spec).await?;
                if let Some(hash) = &hash {
                    self.store().users().set_password(id, hash).await?;
                }
                id
            }
            None => {
                self.store()
                    .users()
                    .create(&spec, hash.as_deref().unwrap_or_default())
                    .await?
            }
        };
        if !spec.is_active {
            self.store().sessions().delete_for_user(id).await?;
        }
        self.audit(format!("User \"{}\" saved", spec.username), None).await;
        Ok(id)
    }

    pub async fn delete_user(&self, id: UserId) -> AppResult<()> {
        self.require_admin()?;
        if id == self.user.id {
            return Err(AppError::Conflict("you cannot delete your own account".into()));
        }
        let user = self.store().users().get(id).await?;
        if user.is_admin() && self.store().users().count_active_admins().await? <= 1 {
            return Err(AppError::Conflict(
                "at least one active administrator is required".into(),
            ));
        }
        self.store().users().delete(id).await?;
        self.audit(format!("User \"{}\" deleted", user.spec.username), None)
            .await;
        Ok(())
    }

    /// Changes the current user's own password after checking the old one.
    pub async fn change_own_password(&self, old: &str, new: &str) -> AppResult<()> {
        if crate::auth::authenticate(self.state, &self.user.spec.username, old)
            .await?
            .is_none()
        {
            return Err(ValidationErrors::single("old_password", "the current password is wrong").into());
        }
        validate_password(new)?;
        let hash = hash_password(new).map_err(AppError::Internal)?;
        self.store().users().set_password(self.user.id, &hash).await?;
        self.audit("Password changed", None).await;
        Ok(())
    }

    // ----- logs ----------------------------------------------------------

    /// Restricts a log filter to what the user may see.
    pub fn log_filter(&self, mut filter: LogFilter) -> LogFilter {
        if !self.is_admin() {
            filter.owner = Some(self.user.id);
        }
        filter
    }
}
