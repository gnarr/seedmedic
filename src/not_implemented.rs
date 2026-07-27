//! Placeholder adapters must fail loudly, not pretend.
//!
//! An adapter that is stubbed out returns this error, naming the TODO document
//! that describes the work. The workflow treats it as "hold for review", so a
//! stubbed integration parks jobs safely instead of silently reporting success.

use thiserror::Error;

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("{adapter} is not implemented yet (see {todo})")]
pub struct NotImplemented {
    pub adapter: &'static str,
    pub todo: &'static str,
}

impl NotImplemented {
    pub const fn new(adapter: &'static str, todo: &'static str) -> Self {
        Self { adapter, todo }
    }
}
