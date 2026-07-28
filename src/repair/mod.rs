//! Repair workflow management: the durable state machine that ties the other
//! capabilities together.
//!
//! Read `src/repair/AGENTS.md` before changing transitions, the store contract,
//! or the worker loop.

pub mod adapters;
pub mod application;
mod domain;
pub mod policy;
mod ports;
pub mod reconcile;
pub mod worker;

pub use domain::{
    InvalidTransition, JobId, RepairJob, RepairState, ReviewReason, Transition, TransitionReason,
    TransitionRecord, UnknownState, validate_transition,
};
pub use policy::{
    AutoResume, MatchDecision, MaterializationPolicy, ResumeDecision, SafetyPolicy, decide_match,
    decide_resume, queued_recheck_poll_delay, recheck_elapsed, recheck_poll_delay, retry_delay,
    retry_delay_with_jitter, tracker_poll_delay,
};
pub use ports::{
    Applied, Discovered, FileCompleteness, JobPatch, PlannedFile, RepairStore, StoreError,
    TransitionUpdate,
};
pub use worker::{RepairDeps, RepairWorker, WorkerConfig, WorkerHealth};
