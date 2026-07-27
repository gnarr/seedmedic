//! Unit3D-family tracker adapter (Blutopia, Aither, and relatives).
//!
//! Not implemented in the bootstrap: see `docs/todos/0002-unit3d-tracker.md`,
//! which covers the API surface, authentication, rate limiting, and how to tell
//! "hit-and-run cleared" from "endpoint changed shape".
//!
//! Every method fails with [`NotImplemented`] so a misconfiguration parks jobs
//! for review instead of quietly reporting zero warnings.

use async_trait::async_trait;
use url::Url;

use crate::{
    not_implemented::NotImplemented,
    tracker::{
        domain::{HitAndRun, HitAndRunStatus, TrackerId, TrackerTorrentId},
        ports::{TrackerClient, TrackerError},
    },
};

const TODO: &str = "docs/todos/0002-unit3d-tracker.md";

pub struct Unit3dTracker {
    id: TrackerId,
    #[allow(
        dead_code,
        reason = "used once docs/todos/0002-unit3d-tracker.md lands"
    )]
    base_url: Url,
}

impl Unit3dTracker {
    pub fn new(id: TrackerId, base_url: Url) -> Self {
        Self { id, base_url }
    }

    fn unimplemented(&self) -> TrackerError {
        NotImplemented::new("tracker::adapters::unit3d", TODO).into()
    }
}

#[async_trait]
impl TrackerClient for Unit3dTracker {
    fn id(&self) -> &TrackerId {
        &self.id
    }

    async fn list_hit_and_runs(&self) -> Result<Vec<HitAndRun>, TrackerError> {
        Err(self.unimplemented())
    }

    async fn fetch_torrent_file(&self, _id: &TrackerTorrentId) -> Result<Vec<u8>, TrackerError> {
        Err(self.unimplemented())
    }

    async fn hit_and_run_status(
        &self,
        _id: &TrackerTorrentId,
    ) -> Result<HitAndRunStatus, TrackerError> {
        Err(self.unimplemented())
    }
}
