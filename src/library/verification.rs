//! Piece-hash verification: the impure stage `plan_matches` cannot be.
//!
//! `plan_matches` picks a candidate from size and name alone, which is
//! evidence, not proof — two different encodes of the same episode can be
//! byte-identical in length. This module hashes a bounded number of a
//! torrent's pieces against a chosen candidate's bytes to turn "probably
//! this one" into "verified".
//!
//! Split in two, deliberately:
//!
//! - Piece-to-file mapping and piece selection are pure functions: given a
//!   piece index and a file's position in the torrent, there is exactly one
//!   right answer, and getting it wrong produces a confident wrong
//!   verification — the worst failure mode available, so they earn their own
//!   tests independent of any I/O.
//! - [`verify_matches`] is the impure rest: it reads bounded byte ranges off
//!   the runtime (`spawn_blocking`), hashes them, and decides what the plan
//!   should say. A mismatch never downgrades a match in place — the candidate
//!   is wrong, so it is dropped and [`plan_matches`] is asked again with what
//!   is left. That retry is bounded by the number of candidates it started
//!   with, since each round removes at least one.

use std::{
    io::{self, Read, Seek, SeekFrom},
    ops::Range,
    path::{Path, PathBuf},
};

use sha1::{Digest, Sha1};
use tokio::task;

use super::{
    domain::{Candidate, MatchConfidence, MatchPlan},
    matching::plan_matches,
};
use crate::torrent::{PieceHash, SafeRelativePath, TorrentFile};

/// The byte range `files[index]` occupies in the concatenated stream a
/// `.torrent`'s pieces are hashed over. Files are laid out back to back, in
/// the order the torrent lists them.
pub fn file_byte_range(files: &[TorrentFile], index: usize) -> Range<u64> {
    let start: u64 = files[..index].iter().map(|file| file.length).sum();
    start..start + files[index].length
}

/// The byte range `piece_index` covers in that same stream. The final piece
/// is shorter than `piece_length` whenever `total_length` is not an exact
/// multiple of it — clamping to `total_length` is what makes that piece's
/// range come out right instead of running past the end of the torrent.
pub fn piece_byte_range(piece_index: usize, piece_length: u64, total_length: u64) -> Range<u64> {
    let start = piece_index as u64 * piece_length;
    let end = (start + piece_length).min(total_length);
    start..end
}

/// Which pieces fall entirely inside `file_range` — the only ones that can be
/// verified against a single candidate file.
///
/// A piece that straddles a file boundary satisfies neither file's
/// containment check, so it is silently absent from both files' lists rather
/// than mishandled. Verifying it would require both files' candidates chosen
/// first; see the open question in `docs/todos/0005-media-matching.md`.
pub fn pieces_within_file(
    file_range: &Range<u64>,
    piece_length: u64,
    piece_count: usize,
    total_length: u64,
) -> Vec<usize> {
    (0..piece_count)
        .filter(|&index| {
            let piece = piece_byte_range(index, piece_length, total_length);
            piece.start >= file_range.start && piece.end <= file_range.end
        })
        .collect()
}

/// Choose up to `count` pieces to check out of the ones `pieces_within_file`
/// found available, spread first, last, and as evenly as possible in
/// between — enough to catch truncation, a wrong encode, or a wrong episode
/// without hashing the whole file. Deterministic, so the same plan always
/// checks the same pieces.
pub fn select_pieces(available: &[usize], count: usize) -> Vec<usize> {
    if count == 0 || available.is_empty() {
        return Vec::new();
    }
    if available.len() <= count {
        return available.to_vec();
    }

    let last = available.len() - 1;
    let step_divisor = (count - 1).max(1);
    let mut chosen: Vec<usize> = (0..count)
        .map(|position| available[position * last / step_divisor])
        .collect();
    chosen.dedup();
    chosen
}

/// One piece hashed against a candidate, whichever way it came out. Kept for
/// the audit trail — "we tried this file and it hashed wrong" is worth
/// showing an operator even after the candidate has been dropped.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PieceCheck {
    pub torrent_path: SafeRelativePath,
    pub candidate: PathBuf,
    pub piece_index: usize,
    pub matched: bool,
}

/// Every piece check made while reaching the returned plan, across every
/// retry round.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VerificationReport {
    pub checks: Vec<PieceCheck>,
}

impl VerificationReport {
    pub fn rejected(&self) -> impl Iterator<Item = &PieceCheck> {
        self.checks.iter().filter(|check| !check.matched)
    }
}

/// Verify `plan_matches`'s choice for every file, upgrading whatever verifies
/// to [`MatchConfidence::Exact`]. A candidate that hashes wrong is dropped and
/// selection is retried without it — a file that hashes wrong is the wrong
/// file, never a lower-confidence match.
///
/// `verification_pieces = 0` or an empty `pieces` list (no piece data
/// available) both skip verification entirely, leaving `plan_matches`'s
/// answer untouched.
pub async fn verify_matches(
    files: &[TorrentFile],
    mut candidates: Vec<Candidate>,
    piece_length: u64,
    pieces: &[PieceHash],
    verification_pieces: usize,
) -> (MatchPlan, VerificationReport) {
    let mut report = VerificationReport::default();

    if verification_pieces == 0 || pieces.is_empty() {
        return (plan_matches(files, &candidates), report);
    }

    let total_length: u64 = files.iter().map(|file| file.length).sum();
    // Each round drops at least one candidate, so this always terminates —
    // the cap only guards against a logic error turning that into a hang.
    let max_rounds = candidates.len() + 1;

    for _ in 0..max_rounds {
        let mut plan = plan_matches(files, &candidates);
        let outcome = verify_round(
            files,
            &plan,
            piece_length,
            pieces,
            total_length,
            verification_pieces,
            &mut report,
        )
        .await;

        match outcome {
            RoundOutcome::Retry(rejected_paths) => {
                candidates.retain(|candidate| !rejected_paths.contains(&candidate.path));
            }
            RoundOutcome::Done(verified) => {
                for index in verified {
                    plan.matched[index].confidence = MatchConfidence::Exact;
                    plan.matched[index].evidence.piece_verified = true;
                }
                return (plan, report);
            }
        }
    }

    (plan_matches(files, &candidates), report)
}

enum RoundOutcome {
    /// At least one chosen candidate hashed wrong; these paths must be
    /// dropped before selection runs again.
    Retry(Vec<PathBuf>),
    /// Nothing hashed wrong. `Vec<usize>` indexes `plan.matched` entries that
    /// verified and should become `Exact`; anything not listed either had no
    /// piece available to check or could not be read.
    Done(Vec<usize>),
}

async fn verify_round(
    files: &[TorrentFile],
    plan: &MatchPlan,
    piece_length: u64,
    pieces: &[PieceHash],
    total_length: u64,
    verification_pieces: usize,
    report: &mut VerificationReport,
) -> RoundOutcome {
    let mut rejected = Vec::new();
    let mut verified = Vec::new();

    for (match_index, matched) in plan.matched.iter().enumerate() {
        let Some(file_index) = files
            .iter()
            .position(|file| file.path == matched.torrent_path)
        else {
            continue;
        };

        let file_range = file_byte_range(files, file_index);
        let available = pieces_within_file(&file_range, piece_length, pieces.len(), total_length);
        let chosen = select_pieces(&available, verification_pieces);
        if chosen.is_empty() {
            continue; // No piece data available for this file: leave it be.
        }

        let mut file_verified = true;
        for piece_index in chosen {
            let piece_range = piece_byte_range(piece_index, piece_length, total_length);
            let relative =
                (piece_range.start - file_range.start)..(piece_range.end - file_range.start);

            let Ok(digest) = hash_piece(matched.source.clone(), relative).await else {
                // Unreadable is "cannot say", not "wrong" — best-effort, per
                // docs/todos/0005-media-matching.md.
                file_verified = false;
                break;
            };

            let piece_matched = digest == *pieces[piece_index].as_bytes();
            report.checks.push(PieceCheck {
                torrent_path: matched.torrent_path.clone(),
                candidate: matched.source.clone(),
                piece_index,
                matched: piece_matched,
            });

            if !piece_matched {
                file_verified = false;
                rejected.push(matched.source.clone());
                break;
            }
        }

        if file_verified {
            verified.push(match_index);
        }
    }

    if rejected.is_empty() {
        RoundOutcome::Done(verified)
    } else {
        RoundOutcome::Retry(rejected)
    }
}

/// Hash the given byte range of `candidate`, off the async runtime — reading
/// and SHA-1 are blocking work, and only the bytes a piece needs are read.
async fn hash_piece(candidate: PathBuf, range: Range<u64>) -> io::Result<[u8; 20]> {
    match task::spawn_blocking(move || read_and_hash(&candidate, range)).await {
        Ok(result) => result,
        Err(join_error) => Err(io::Error::other(join_error)),
    }
}

fn read_and_hash(path: &Path, range: Range<u64>) -> io::Result<[u8; 20]> {
    let bytes = read_range(path, range)?;
    Ok(Sha1::digest(&bytes).into())
}

/// Seek plus a bounded read: never more of the candidate than the piece
/// itself, however large the file is.
fn read_range(path: &Path, range: Range<u64>) -> io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(range.start))?;
    let len = usize::try_from(range.end - range.start)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let mut buffer = vec![0u8; len];
    file.read_exact(&mut buffer)?;
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(length: u64) -> TorrentFile {
        TorrentFile {
            path: crate::torrent::SafeRelativePath::parse("f").expect("valid"),
            length,
        }
    }

    #[test]
    fn file_byte_range_accounts_for_earlier_files() {
        let files = vec![file(100), file(200), file(50)];
        assert_eq!(file_byte_range(&files, 0), 0..100);
        assert_eq!(file_byte_range(&files, 1), 100..300);
        assert_eq!(file_byte_range(&files, 2), 300..350);
    }

    #[test]
    fn piece_byte_range_is_regular_except_for_the_final_short_piece() {
        // 100 bytes at 30 per piece: three full pieces, one 10-byte remainder.
        assert_eq!(piece_byte_range(0, 30, 100), 0..30);
        assert_eq!(piece_byte_range(1, 30, 100), 30..60);
        assert_eq!(piece_byte_range(2, 30, 100), 60..90);
        assert_eq!(piece_byte_range(3, 30, 100), 90..100);
    }

    #[test]
    fn single_file_torrent_contains_every_piece() {
        let files = vec![file(100)];
        let range = file_byte_range(&files, 0);
        assert_eq!(pieces_within_file(&range, 30, 4, 100), vec![0, 1, 2, 3]);
    }

    #[test]
    fn a_piece_spanning_a_file_boundary_belongs_to_neither_file() {
        // Two 100-byte files, 30-byte pieces, 200 total: piece 3 covers
        // bytes 90..120, which crosses the 100-byte boundary between them.
        let files = vec![file(100), file(100)];
        let piece_count = 7; // ceil(200 / 30)
        let total = 200;

        let first = pieces_within_file(&file_byte_range(&files, 0), 30, piece_count, total);
        let second = pieces_within_file(&file_byte_range(&files, 1), 30, piece_count, total);

        assert_eq!(first, vec![0, 1, 2]);
        assert_eq!(second, vec![4, 5, 6]);
        assert!(!first.contains(&3));
        assert!(!second.contains(&3));
    }

    #[test]
    fn the_final_short_piece_still_counts_when_it_fits_inside_a_file() {
        let files = vec![file(100), file(100)];
        let piece_count = 7;
        let total = 200;

        let second = pieces_within_file(&file_byte_range(&files, 1), 30, piece_count, total);

        // Piece 6 covers 180..200, the short remainder, and sits entirely
        // inside the second file (100..200).
        assert!(second.contains(&6));
    }

    #[test]
    fn a_file_smaller_than_one_piece_has_no_fully_contained_piece() {
        // First file is 10 bytes; the only piece touching it (0..30) reaches
        // into the next file, so nothing verifies it alone.
        let files = vec![file(10), file(1000)];
        let piece_count = 34; // ceil(1010 / 30)
        let total = 1010;

        let first = pieces_within_file(&file_byte_range(&files, 0), 30, piece_count, total);
        assert!(first.is_empty());
    }

    #[test]
    fn select_pieces_is_empty_when_disabled_or_nothing_is_available() {
        assert_eq!(select_pieces(&[1, 2, 3], 0), Vec::<usize>::new());
        assert_eq!(select_pieces(&[], 3), Vec::<usize>::new());
    }

    #[test]
    fn select_pieces_takes_everything_when_fewer_are_available_than_requested() {
        assert_eq!(select_pieces(&[10, 20], 3), vec![10, 20]);
    }

    #[test]
    fn select_pieces_spreads_first_middle_last() {
        assert_eq!(select_pieces(&[1, 2, 3, 4, 5], 3), vec![1, 3, 5]);
        assert_eq!(select_pieces(&[1, 2, 3, 4, 5], 1), vec![1]);
    }

    #[test]
    fn select_pieces_is_deterministic() {
        let available: Vec<usize> = (0..50).collect();
        assert_eq!(select_pieces(&available, 3), select_pieces(&available, 3));
    }

    use crate::library::domain::CandidateOrigin;

    fn torrent_file(path: &str, length: u64) -> TorrentFile {
        TorrentFile {
            path: SafeRelativePath::parse(path).expect("valid test path"),
            length,
        }
    }

    fn candidate(path: PathBuf, size_bytes: u64) -> Candidate {
        Candidate {
            path,
            size_bytes,
            origin: CandidateOrigin::Filesystem {
                root: PathBuf::from("/media"),
            },
        }
    }

    fn piece_hash(bytes: &[u8]) -> PieceHash {
        PieceHash::from_bytes(Sha1::digest(bytes).into())
    }

    #[tokio::test]
    async fn a_candidate_with_the_right_bytes_verifies_to_exact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let content = vec![b'a'; 100];
        let path = dir.path().join("e01.mkv");
        std::fs::write(&path, &content).expect("write candidate");

        let files = vec![torrent_file("Show/e01.mkv", 100)];
        let candidates = vec![candidate(path.clone(), 100)];
        let pieces = vec![piece_hash(&content)];

        let (plan, report) = verify_matches(&files, candidates, 100, &pieces, 3).await;

        assert_eq!(plan.matched[0].source, path);
        assert_eq!(plan.matched[0].confidence, MatchConfidence::Exact);
        assert!(plan.matched[0].evidence.piece_verified);
        assert!(!report.checks.is_empty());
        assert!(report.rejected().next().is_none());
    }

    #[tokio::test]
    async fn a_wrong_candidate_is_rejected_and_a_correct_second_one_is_chosen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let correct = vec![b'a'; 100];
        let wrong = vec![b'b'; 100];

        // Named the way the torrent wants, but the wrong bytes.
        let wrong_path = dir.path().join("e01.mkv");
        // Right bytes, but renamed — plausible after an *arr import.
        let correct_path = dir.path().join("renamed.mkv");
        std::fs::write(&wrong_path, &wrong).expect("write wrong candidate");
        std::fs::write(&correct_path, &correct).expect("write correct candidate");

        let files = vec![torrent_file("Show/e01.mkv", 100)];
        let candidates = vec![
            candidate(wrong_path.clone(), 100),
            candidate(correct_path.clone(), 100),
        ];
        let pieces = vec![piece_hash(&correct)];

        let (plan, report) = verify_matches(&files, candidates, 100, &pieces, 3).await;

        assert_eq!(plan.matched[0].source, correct_path);
        assert_eq!(plan.matched[0].confidence, MatchConfidence::Exact);
        assert!(report.rejected().any(|check| check.candidate == wrong_path));
    }

    #[tokio::test]
    async fn every_candidate_rejected_leaves_the_plan_incomplete() {
        let dir = tempfile::tempdir().expect("tempdir");
        let wrong = vec![b'b'; 100];
        let path = dir.path().join("e01.mkv");
        std::fs::write(&path, &wrong).expect("write candidate");

        let files = vec![torrent_file("Show/e01.mkv", 100)];
        let candidates = vec![candidate(path, 100)];
        let pieces = vec![piece_hash(&[b'a'; 100])];

        let (plan, _report) = verify_matches(&files, candidates, 100, &pieces, 3).await;

        assert!(!plan.is_complete());
        assert!(plan.matched.is_empty());
    }

    #[tokio::test]
    async fn zero_verification_pieces_matches_todays_behaviour() {
        let dir = tempfile::tempdir().expect("tempdir");
        let content = vec![b'a'; 100];
        let path = dir.path().join("e01.mkv");
        std::fs::write(&path, &content).expect("write candidate");

        let files = vec![torrent_file("Show/e01.mkv", 100)];
        let candidates = vec![candidate(path, 100)];
        let pieces = vec![piece_hash(&content)];

        let (plan, report) = verify_matches(&files, candidates, 100, &pieces, 0).await;

        assert_eq!(plan.matched[0].confidence, MatchConfidence::Probable);
        assert!(!plan.matched[0].evidence.piece_verified);
        assert!(report.checks.is_empty());
    }

    #[tokio::test]
    async fn an_unreadable_candidate_degrades_to_probable_not_an_error() {
        let files = vec![torrent_file("Show/e01.mkv", 100)];
        let missing = PathBuf::from("/does/not/exist/e01.mkv");
        let candidates = vec![candidate(missing.clone(), 100)];
        let pieces = vec![piece_hash(&[b'a'; 100])];

        let (plan, report) = verify_matches(&files, candidates, 100, &pieces, 3).await;

        assert_eq!(plan.matched[0].source, missing);
        assert_eq!(plan.matched[0].confidence, MatchConfidence::Probable);
        assert!(report.checks.is_empty());
    }

    #[test]
    fn verifying_a_piece_of_a_huge_file_reads_far_less_than_the_whole_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("huge.mkv");
        let file = std::fs::File::create(&path).expect("create");
        file.set_len(1 << 30).expect("sparse 1 GiB file");
        drop(file);

        let bytes = read_range(&path, 12_345..12_345 + 16_384).expect("bounded read");

        assert_eq!(bytes.len(), 16_384);
        assert!((bytes.len() as u64) < (1 << 30));
    }
}
