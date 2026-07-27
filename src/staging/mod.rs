//! Recovery staging: the only place SeedMedic writes to a filesystem.
//!
//! Read `src/staging/AGENTS.md` before changing anything here. The short
//! version: the media library is read-only, the staging root is validated to be
//! somewhere else entirely, and every destination path is resolved through
//! [`safety::resolve_under`] before it is touched.

pub mod adapters;
mod domain;
mod ports;
pub mod safety;

pub use domain::{
    MaterializationPlan, MaterializationStrategy, PlanItem, StagedFile, StagedLayout,
    StagingPresence, StagingRoot, StagingRootError,
};
pub use ports::{StagingError, StagingFilesystem};
