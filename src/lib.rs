//! SeedMedic repairs hit-and-run warnings on private trackers using media the
//! user already has.
//!
//! The top level names business capabilities, not layers. Each capability owns
//! its domain types, its ports, and the adapters that implement them; `repair`
//! is the one that composes the others into a durable workflow.
//!
//! Read `AGENTS.md` before changing the architecture, and the localised
//! `AGENTS.md` files under `src/repair`, `src/tracker`, and `src/staging`
//! before changing those.

// Capabilities.
pub mod library;
pub mod repair;
pub mod seeding;
pub mod staging;
pub mod torrent;
pub mod tracker;

// Driving adapter.
pub mod web;

// Cross-cutting support.
pub mod bootstrap;
pub mod clock;
pub mod config;
pub mod database;
pub mod diagnostics;
#[cfg(feature = "metrics")]
pub mod metrics;
pub mod not_implemented;
