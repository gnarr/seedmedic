use async_trait::async_trait;
use thiserror::Error;

use super::domain::NotificationEvent;

#[derive(Clone, Debug, Error)]
pub enum NotifyError {
    #[error("notification request failed: {0}")]
    Transport(String),
}

/// Fire-and-forget on purpose: a failed notification is logged by the caller
/// and nothing else, never retried and never a reason to change what the
/// worker does next.
#[async_trait]
pub trait Notifier: Send + Sync {
    async fn notify(&self, event: &NotificationEvent) -> Result<(), NotifyError>;
}
