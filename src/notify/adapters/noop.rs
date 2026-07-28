//! The default when `notifications.webhook_url` is unset: every call site
//! can hold an `Arc<dyn Notifier>` unconditionally instead of an `Option`.

use async_trait::async_trait;

use crate::notify::{NotificationEvent, Notifier, NotifyError};

pub struct NoopNotifier;

#[async_trait]
impl Notifier for NoopNotifier {
    async fn notify(&self, _event: &NotificationEvent) -> Result<(), NotifyError> {
        Ok(())
    }
}
