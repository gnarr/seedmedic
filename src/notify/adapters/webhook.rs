//! A generic webhook: a plain JSON POST, body from
//! [`NotificationEvent::to_json`]. Apprise and most "generic webhook"
//! receivers accept exactly this without further configuration.

use async_trait::async_trait;
use url::Url;

use crate::notify::{NotificationEvent, Notifier, NotifyError};

pub struct WebhookNotifier {
    url: Url,
    http: reqwest::Client,
}

impl WebhookNotifier {
    pub fn new(url: Url, http: reqwest::Client) -> Self {
        Self { url, http }
    }
}

#[async_trait]
impl Notifier for WebhookNotifier {
    async fn notify(&self, event: &NotificationEvent) -> Result<(), NotifyError> {
        let response = self
            .http
            .post(self.url.clone())
            .json(&event.to_json())
            .send()
            .await
            .map_err(|error| NotifyError::Transport(error.without_url().to_string()))?;

        if response.status().is_success() {
            Ok(())
        } else {
            Err(NotifyError::Transport(format!(
                "webhook returned status {}",
                response.status()
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_partial_json, method, path},
    };

    use super::*;
    use crate::repair::JobId;

    #[tokio::test]
    async fn posts_the_event_as_json() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .and(body_partial_json(serde_json::json!({
                "event": "completed",
                "job": 1,
            })))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let notifier = WebhookNotifier::new(
            Url::parse(&server.uri()).expect("mock server URI parses"),
            reqwest::Client::new(),
        );

        notifier
            .notify(&NotificationEvent::Completed {
                job: JobId(1),
                tracker: "example".to_owned(),
                torrent_name: "Demo".to_owned(),
            })
            .await
            .expect("mocked webhook accepts the request");
    }

    #[tokio::test]
    async fn a_non_success_status_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let notifier = WebhookNotifier::new(
            Url::parse(&server.uri()).expect("mock server URI parses"),
            reqwest::Client::new(),
        );

        let error = notifier
            .notify(&NotificationEvent::TrackerUnreachable {
                tracker: "example".to_owned(),
                unreachable_for: std::time::Duration::from_secs(1800),
            })
            .await
            .expect_err("a 500 is an error");
        assert!(matches!(error, NotifyError::Transport(_)));
    }
}
