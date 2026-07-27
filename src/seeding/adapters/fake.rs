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
        domain::{AddTorrent, ClientTorrentState, DataCompleteness, FileProgress, TorrentStatus},
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
    /// Set once a forced state (`Errored`, via [`FakeTorrentClient::set_errored`])
    /// overrides whatever a recheck would otherwise report.
    message: Option<String>,
}

#[derive(Default)]
pub struct FakeTorrentClient {
    torrents: Mutex<HashMap<InfoHash, Entry>>,
    /// What a recheck will conclude, per torrent. Absent means `Complete`.
    on_disk: Mutex<HashMap<InfoHash, DataCompleteness>>,
    /// Per-file breakdown a `status` call reports, once set. Absent means
    /// `TorrentStatus::files` is `None`, exercising the "client offers no
    /// per-file detail" path.
    file_progress: Mutex<HashMap<InfoHash, Vec<FileProgress>>>,
    recheck_polls: Mutex<usize>,
    /// Whether a `Checking` torrent should report itself as queued rather
    /// than actively running.
    queued: Mutex<HashMap<InfoHash, bool>>,
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
    ///
    /// Takes effect on the next `status` call once the torrent is not
    /// `Checking`, live rather than cached — so a test can change it between
    /// the `verify` step's read and the `resume` step's, and prove the latter
    /// does not trust the former.
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

    /// The per-file breakdown a recheck will report for this torrent, once it
    /// finishes. Not setting this leaves `TorrentStatus::files` at `None`.
    pub fn set_file_progress(&self, info_hash: InfoHash, files: Vec<FileProgress>) {
        self.file_progress
            .lock()
            .expect("fake client poisoned")
            .insert(info_hash, files);
    }

    /// Report a running check as queued rather than actively checking, so
    /// tests can exercise the longer poll interval that implies.
    pub fn set_queued(&self, info_hash: InfoHash, queued: bool) {
        self.queued
            .lock()
            .expect("fake client poisoned")
            .insert(info_hash, queued);
    }

    /// Force the torrent into `Errored`, as if the client hit a disk error or
    /// missing file mid-check, carrying `message` in the next `status` call.
    pub fn set_errored(&self, info_hash: InfoHash, message: impl Into<String>) {
        let mut torrents = self.lock();
        if let Some(entry) = torrents.get_mut(&info_hash) {
            entry.state = ClientTorrentState::Errored;
            entry.message = Some(message.into());
        }
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
                message: None,
            },
        );
        Ok(())
    }

    async fn status(&self, info_hash: InfoHash) -> Result<Option<TorrentStatus>, ClientError> {
        if let Some(error) = self.take_error() {
            return Err(error);
        }

        let resolved = self.completeness_of(info_hash);
        let queued = self
            .queued
            .lock()
            .expect("fake client poisoned")
            .get(&info_hash)
            .copied()
            .unwrap_or(false);
        let files = self
            .file_progress
            .lock()
            .expect("fake client poisoned")
            .get(&info_hash)
            .cloned();
        let mut torrents = self.lock();
        let Some(entry) = torrents.get_mut(&info_hash) else {
            return Ok(None);
        };

        if entry.state == ClientTorrentState::Checking {
            if entry.checks_remaining > 0 {
                entry.checks_remaining -= 1;
            } else {
                entry.state = ClientTorrentState::Paused;
            }
        }

        // Live, not cached: a real client re-derives progress on every poll,
        // and `resume` relies on this to catch data that changed after
        // `verify` already read it — see `set_on_disk`'s doc comment.
        let completeness = if entry.state == ClientTorrentState::Checking {
            entry.completeness
        } else {
            resolved
        };

        Ok(Some(TorrentStatus {
            state: entry.state,
            completeness,
            save_path: entry.save_path.clone(),
            files,
            queued: entry.state == ClientTorrentState::Checking && queued,
            message: entry.message.clone(),
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

        let resolved = self.completeness_of(info_hash);
        let mut torrents = self.lock();
        let Some(entry) = torrents.get_mut(&info_hash) else {
            return Err(ClientError::Rejected("unknown torrent".into()));
        };

        if entry.state != ClientTorrentState::Seeding {
            self.resumed.fetch_add(1, Ordering::SeqCst);
            entry.state = if resolved.is_complete() {
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
