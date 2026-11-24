//! Email notifications of job results.

use async_trait::async_trait;
use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

/// A message ready to be delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub to: Vec<String>,
    pub subject: String,
    pub body: String,
}

#[async_trait]
pub trait Notifier: Send + Sync {
    async fn send(&self, notification: Notification) -> Result<(), String>;
}

/// Drops every notification; used when no mail server is configured.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullNotifier;

#[async_trait]
impl Notifier for NullNotifier {
    async fn send(&self, notification: Notification) -> Result<(), String> {
        tracing::debug!(to = ?notification.to, subject = %notification.subject, "mail disabled, notification dropped");
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct SmtpSettings {
    pub host: String,
    pub port: u16,
    /// `none`, `starttls` or `tls`.
    pub security: SmtpSecurity,
    pub username: Option<String>,
    pub password: Option<String>,
    pub from: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SmtpSecurity {
    None,
    Starttls,
    Tls,
}

pub struct SmtpNotifier {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
}

impl SmtpNotifier {
    pub fn new(settings: &SmtpSettings) -> Result<Self, String> {
        let builder = match settings.security {
            SmtpSecurity::None => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&settings.host),
            SmtpSecurity::Starttls => {
                AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&settings.host).map_err(|e| e.to_string())?
            }
            SmtpSecurity::Tls => {
                AsyncSmtpTransport::<Tokio1Executor>::relay(&settings.host).map_err(|e| e.to_string())?
            }
        };
        let mut builder = builder.port(settings.port);
        if let (Some(user), Some(password)) = (&settings.username, &settings.password) {
            builder = builder.credentials(Credentials::new(user.clone(), password.clone()));
        }
        Ok(Self {
            transport: builder.build(),
            from: settings
                .from
                .parse()
                .map_err(|e| format!("invalid from address: {e}"))?,
        })
    }
}

#[async_trait]
impl Notifier for SmtpNotifier {
    async fn send(&self, notification: Notification) -> Result<(), String> {
        let mut builder = Message::builder().from(self.from.clone()).subject(notification.subject);
        for to in &notification.to {
            builder = builder.to(to.parse().map_err(|e| format!("invalid recipient {to}: {e}"))?);
        }
        let message = builder.body(notification.body).map_err(|e| e.to_string())?;
        self.transport.send(message).await.map_err(|e| e.to_string())?;
        Ok(())
    }
}
