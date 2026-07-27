//! An in-memory download client that behaves like one.
//!
//! It keeps torrents paused until told otherwise, takes a configurable number
//! of polls to finish a recheck, and reports whatever completeness the test
//! asked for. Call counts are exposed so tests can assert that a repair which
//! crashed and restarted did not add the same torrent twice.

use std::{
    collections::HashMap,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;

use crate::{
    seeding::{
        domain::{AddTorrent, ClientTorrentState, DataCompleteness, TorrentStatus},
        ports::{ClientError, TorrentClient},
    },
    torrent::InfoHash,
};

struct Entry {
    state: ClientTorrentState,
    completeness: DataCompleteness,
    save_path: std::path::PathBuf,
    /// Polls remaining before a recheck finishes.
    checks_remaining: usize,
}

#[derive(Default)]
pub struct FakeTorrentClient {
    torrents: Mutex<HashMap<InfoHash, Entry>>,
    /// What a recheck will conclude, per torrent. Absent means `Complete`.
    on_disk: Mutex<HashMap<InfoHash, DataCompleteness>>,
    recheck_polls: Mutex<usize>,
    added: AtomicUsize,
    rechecked: AtomicUsize,
    resumed: AtomicUsize,
    next_error: Mutex<Option<ClientError>>,
}

impl FakeTorrentClient {
    pub fn new() -> Self {
        Self {
            recheck_polls: Mutex::new(1),
            ..Self::default()
        }
    }

    /// Decide what a recheck of this torrent will find. Use `Partial` to
    /// exercise the "do not resume incomplete data" paths.
    pub fn set_on_disk(&self, info_hash: InfoHash, completeness: DataCompleteness) {
        self.on_disk
            .lock()
            .expect("fake client poisoned")
            .insert(info_hash, completeness);
    }

    /// How many `status` polls a recheck takes to finish. Zero makes rechecks
    /// instant; the default of one forces the workflow through its "still
    /// checking" branch.
    pub fn set_recheck_polls(&self, polls: usize) {
        *self.recheck_polls.lock().expect("fake client poisoned") = polls;
    }

    pub fn fail_next_call_with(&self, error: ClientError) {
        *self.next_error.lock().expect("fake client poisoned") = Some(error);
    }

    /// Simulate an operator removing the torrent behind SeedMedic's back.
    pub fn forget(&self, info_hash: InfoHash) {
        self.lock().remove(&info_hash);
    }

    pub fn add_count(&self) -> usize {
        self.added.load(Ordering::SeqCst)
    }

    pub fn recheck_count(&self) -> usize {
        self.rechecked.load(Ordering::SeqCst)
    }

    pub fn resume_count(&self) -> usize {
        self.resumed.load(Ordering::SeqCst)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<InfoHash, Entry>> {
        self.torrents.lock().expect("fake client poisoned")
    }

    fn take_error(&self) -> Option<ClientError> {
        self.next_error.lock().expect("fake client poisoned").take()
    }

    fn completeness_of(&self, info_hash: InfoHash) -> DataCompleteness {
        self.on_disk
            .lock()
            .expect("fake client poisoned")
            .get(&info_hash)
            .copied()
            .unwrap_or(DataCompleteness::Complete)
    }
}

#[async_trait]
impl TorrentClient for FakeTorrentClient {
    async fn add_paused(&self, request: AddTorrent<'_>) -> Result<(), ClientError> {
        if let Some(error) = self.take_error() {
            return Err(error);
        }
        if request.torrent_file.is_empty() {
            return Err(ClientError::Rejected("empty torrent file".into()));
        }

        let mut torrents = self.lock();
        // Re-adding is a no-op, exactly as the port requires.
        if torrents.contains_key(&request.info_hash) {
            return Ok(());
        }

        self.added.fetch_add(1, Ordering::SeqCst);
        torrents.insert(
            request.info_hash,
            Entry {
                state: ClientTorrentState::Paused,
                // Nothing is known about the data until a recheck happens.
                completeness: DataCompleteness::Partial { ratio: 0.0 },
                save_path: request.save_path.to_path_buf(),
                checks_remaining: 0,
            },
        );
        Ok(())
    }

    async fn status(&self, info_hash: InfoHash) -> Result<Option<TorrentStatus>, ClientError> {
        if let Some(error) = self.take_error() {
            return Err(error);
        }

        let resolved = self.completeness_of(info_hash);
        let mut torrents = self.lock();
        let Some(entry) = torrents.get_mut(&info_hash) else {
            return Ok(None);
        };

        if entry.state == ClientTorrentState::Checking {
            if entry.checks_remaining > 0 {
                entry.checks_remaining -= 1;
            } else {
                entry.state = ClientTorrentState::Paused;
                entry.completeness = resolved;
            }
        }

        Ok(Some(TorrentStatus {
            state: entry.state,
            completeness: entry.completeness,
            save_path: entry.save_path.clone(),
        }))
    }

    async fn recheck(&self, info_hash: InfoHash) -> Result<(), ClientError> {
        if let Some(error) = self.take_error() {
            return Err(error);
        }

        let polls = *self.recheck_polls.lock().expect("fake client poisoned");
        let mut torrents = self.lock();
        let Some(entry) = torrents.get_mut(&info_hash) else {
            return Err(ClientError::Rejected("unknown torrent".into()));
        };

        // Re-issuing a recheck that is already running is a no-op.
        if entry.state != ClientTorrentState::Checking {
            self.rechecked.fetch_add(1, Ordering::SeqCst);
            entry.state = ClientTorrentState::Checking;
            entry.checks_remaining = polls;
        }
        Ok(())
    }

    async fn resume(&self, info_hash: InfoHash) -> Result<(), ClientError> {
        if let Some(error) = self.take_error() {
            return Err(error);
        }

        let mut torrents = self.lock();
        let Some(entry) = torrents.get_mut(&info_hash) else {
            return Err(ClientError::Rejected("unknown torrent".into()));
        };

        if entry.state != ClientTorrentState::Seeding {
            self.resumed.fetch_add(1, Ordering::SeqCst);
            entry.state = if entry.completeness.is_complete() {
                ClientTorrentState::Seeding
            } else {
                ClientTorrentState::Downloading
            };
        }
        Ok(())
    }

    async fn remove(&self, info_hash: InfoHash, _delete_files: bool) -> Result<(), ClientError> {
        if let Some(error) = self.take_error() {
            return Err(error);
        }
        self.lock().remove(&info_hash);
        Ok(())
    }
}
