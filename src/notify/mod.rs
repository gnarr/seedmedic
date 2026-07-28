//! Notifications for the events an operator actually cares about: a repair
//! parked for review, a repair completed, a tracker unreachable for a while.
//!
//! Optional and off by default — see `docs/todos/0012-observability.md`. A
//! notifier is never a dependency of the repair workflow: every call site
//! logs and moves on if sending fails, and nothing here ever changes what the
//! worker decides.

pub mod adapters;
mod domain;
mod ports;

pub use domain::NotificationEvent;
pub use ports::{Notifier, NotifyError};
