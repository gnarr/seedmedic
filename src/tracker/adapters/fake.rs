//! An in-memory tracker that behaves like a tracker.
//!
//! It serves warnings, hands out torrent bytes, and — importantly — keeps a
//! hit-and-run `Active` until it is explicitly cleared. That last part is what
//! makes the workflow test meaningful: nothing about the download client can
//! make this tracker say `Cleared`.

use std::{
    collections::HashMap,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;

use crate::tracker::{
    domain::{HitAndRun, HitAndRunStatus, TrackerId, TrackerTorrentId},
    ports::{TrackerClient, TrackerError},
};

/// One torrent the fake tracker knows about.
pub struct FakeTorrent {
    pub hit_and_run: HitAndRun,
    /// What `fetch_torrent_file` returns. Pair with
    /// `torrent::adapters::fake::FakeInspector::encode`.
    pub bytes: Vec<u8>,
}

struct Entry {
    hit_and_run: HitAndRun,
    bytes: Vec<u8>,
    status: HitAndRunStatus,
}

pub struct FakeTracker {
    id: TrackerId,
    entries: Mutex<HashMap<TrackerTorrentId, Entry>>,
    fetches: AtomicUsize,
    /// Set to make the next call fail, to exercise retry paths.
    next_error: Mutex<Option<TrackerError>>,
}

impl FakeTracker {
    pub fn new(id: TrackerId, torrents: Vec<FakeTorrent>) -> Self {
        let entries = torrents
            .into_iter()
            .map(|torrent| {
                (
                    torrent.hit_and_run.torrent_id.clone(),
                    Entry {
                        hit_and_run: torrent.hit_and_run,
                        bytes: torrent.bytes,
                        status: HitAndRunStatus::Active,
                    },
                )
            })
            .collect();

        Self {
            id,
            entries: Mutex::new(entries),
            fetches: AtomicUsize::new(0),
            next_error: Mutex::new(None),
        }
    }

    /// Simulate the tracker deciding the hit-and-run has been satisfied.
    pub fn clear_hit_and_run(&self, id: &TrackerTorrentId) {
        if let Some(entry) = self.lock().get_mut(id) {
            entry.status = HitAndRunStatus::Cleared;
        }
    }

    pub fn fail_next_call_with(&self, error: TrackerError) {
        *self.next_error.lock().expect("fake tracker poisoned") = Some(error);
    }

    /// How many times the `.torrent` was downloaded. A repair that re-fetches
    /// after a crash is wasteful; one that re-fetches every tick is a bug.
    pub fn fetch_count(&self) -> usize {
        self.fetches.load(Ordering::SeqCst)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<TrackerTorrentId, Entry>> {
        self.entries.lock().expect("fake tracker poisoned")
    }

    fn take_error(&self) -> Option<TrackerError> {
        self.next_error
            .lock()
            .expect("fake tracker poisoned")
            .take()
    }
}

#[async_trait]
impl TrackerClient for FakeTracker {
    fn id(&self) -> &TrackerId {
        &self.id
    }

    async fn list_hit_and_runs(&self) -> Result<Vec<HitAndRun>, TrackerError> {
        if let Some(error) = self.take_error() {
            return Err(error);
        }
        Ok(self
            .lock()
            .values()
            .filter(|entry| entry.status == HitAndRunStatus::Active)
            .map(|entry| entry.hit_and_run.clone())
            .collect())
    }

    async fn fetch_torrent_file(&self, id: &TrackerTorrentId) -> Result<Vec<u8>, TrackerError> {
        if let Some(error) = self.take_error() {
            return Err(error);
        }
        self.fetches.fetch_add(1, Ordering::SeqCst);
        self.lock()
            .get(id)
            .map(|entry| entry.bytes.clone())
            .ok_or_else(|| TrackerError::NotFound(id.clone()))
    }

    async fn hit_and_run_status(
        &self,
        id: &TrackerTorrentId,
    ) -> Result<HitAndRunStatus, TrackerError> {
        if let Some(error) = self.take_error() {
            return Err(error);
        }
        self.lock()
            .get(id)
            .map(|entry| entry.status)
            .ok_or_else(|| TrackerError::NotFound(id.clone()))
    }
}
