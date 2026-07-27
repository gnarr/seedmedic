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
//! - Piece-to-file mapping and piece selection (below) are pure functions:
//!   given a piece index and a file's position in the torrent, there is
//!   exactly one right answer, and getting it wrong produces a confident
//!   wrong verification — the worst failure mode available, so it earns its
//!   own tests independent of any I/O.
//! - Actually reading and hashing a candidate's bytes needs the filesystem,
//!   and lands in a later change alongside `plan_matches`'s retry loop.

use std::ops::Range;

use crate::torrent::TorrentFile;

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
}
