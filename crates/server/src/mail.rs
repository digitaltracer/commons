use lettre::message::Mailbox;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

#[async_trait::async_trait]
pub trait Mailer: Send + Sync {
    async fn send(&self, to: &str, subject: &str, body: &str) -> anyhow::Result<()>;
}

/// SMTP delivery through lettre.
pub struct SmtpMailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
}

impl SmtpMailer {
    pub fn new(url: &str, from: &str) -> anyhow::Result<Self> {
        Ok(SmtpMailer {
            transport: AsyncSmtpTransport::<Tokio1Executor>::from_url(url)?.build(),
            from: from.parse()?,
        })
    }
}

#[async_trait::async_trait]
impl Mailer for SmtpMailer {
    async fn send(&self, to: &str, subject: &str, body: &str) -> anyhow::Result<()> {
        let msg = Message::builder()
            .from(self.from.clone())
            .to(to.parse()?)
            .subject(subject)
            .body(body.to_string())?;
        self.transport.send(msg).await?;
        Ok(())
    }
}

/// Used when no SMTP server is configured: logs that mail was due, and keeps nothing.
pub struct LogMailer;

#[async_trait::async_trait]
impl Mailer for LogMailer {
    async fn send(&self, to: &str, subject: &str, _body: &str) -> anyhow::Result<()> {
        tracing::warn!(to, subject, "mail not sent: no SMTP configured");
        Ok(())
    }
}

/// Keeps mail in memory, for tests.
#[cfg(feature = "testing")]
#[derive(Default)]
pub struct MemoryMailer {
    pub sent: std::sync::Mutex<Vec<(String, String, String)>>,
}

#[cfg(feature = "testing")]
#[async_trait::async_trait]
impl Mailer for MemoryMailer {
    async fn send(&self, to: &str, subject: &str, body: &str) -> anyhow::Result<()> {
        tracing::info!(to, subject, "mail (no SMTP configured)");
        if let Ok(mut s) = self.sent.lock() {
            s.push((to.into(), subject.into(), body.into()));
        }
        Ok(())
    }
}
