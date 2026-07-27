//! Deterministic matching of torrent files to library files.
//!
//! Mechanics only: this module decides *which* file is the best candidate and
//! *how sure* it is. Whether that is good enough to act on is a policy question
//! answered in `repair::policy`.
//!
//! Determinism matters — the same inputs must always produce the same plan, so
//! a re-run after a crash reaches the same conclusion and the audit trail stays
//! meaningful. Candidates are therefore sorted before selection.

use crate::torrent::TorrentFile;

use super::domain::{
    Candidate, FileMatch, MatchConfidence, MatchEvidence, MatchPlan, UnmatchedFile, UnmatchedReason,
};

/// Pair every torrent file with the best library candidate we can justify.
pub fn plan_matches(files: &[TorrentFile], candidates: &[Candidate]) -> MatchPlan {
    let mut plan = MatchPlan::default();

    for file in files {
        let wanted_name = file
            .path
            .as_str()
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_owned();

        // Exact size is the only hard filter we have without reading bytes. It
        // is evidence, not proof, so it never buys more than `Probable`.
        let mut sized: Vec<&Candidate> = candidates
            .iter()
            .filter(|candidate| candidate.size_bytes == file.length)
            .collect();
        sized.sort_by(|left, right| left.path.cmp(&right.path));

        let name_matches = |candidate: &Candidate| names_agree(candidate.file_name(), &wanted_name);
        let named: Vec<&Candidate> = sized
            .iter()
            .copied()
            .filter(|candidate| name_matches(candidate))
            .collect();

        let (chosen, confidence) = match (sized.len(), named.len()) {
            (0, _) => {
                plan.unmatched.push(UnmatchedFile {
                    torrent_path: file.path.clone(),
                    length: file.length,
                    reason: UnmatchedReason::NoCandidate,
                });
                continue;
            }
            // Exactly one candidate agreeing on size and name.
            (1, 1) => (sized[0], MatchConfidence::Probable),
            // Several right-sized files but only one with the right name: still
            // the obvious pick, but the collision is recorded in the evidence.
            (_, 1) => (named[0], MatchConfidence::Probable),
            // One right-sized file whose name disagrees: plausible, unproven.
            (1, 0) => (sized[0], MatchConfidence::Ambiguous),
            (count, _) => {
                plan.unmatched.push(UnmatchedFile {
                    torrent_path: file.path.clone(),
                    length: file.length,
                    reason: UnmatchedReason::Ambiguous { candidates: count },
                });
                continue;
            }
        };

        plan.matched.push(FileMatch {
            torrent_path: file.path.clone(),
            length: file.length,
            source: chosen.path.clone(),
            origin: chosen.origin.clone(),
            confidence,
            evidence: MatchEvidence {
                size_matches: true,
                name_matches: name_matches(chosen),
                candidates_with_matching_size: sized.len(),
                piece_verified: false,
            },
        });
    }

    plan
}

/// Filenames agree if they are equal ignoring case. Deliberately strict: the
/// looser comparisons (scene-name normalisation, fuzzy distance) belong with
/// piece verification in `docs/todos/0005-media-matching.md`, where a wrong
/// guess can be caught rather than trusted.
fn names_agree(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{library::domain::CandidateOrigin, torrent::SafeRelativePath};

    fn torrent_file(path: &str, length: u64) -> TorrentFile {
        TorrentFile {
            path: SafeRelativePath::parse(path).expect("valid test path"),
            length,
        }
    }

    fn candidate(path: &str, size_bytes: u64) -> Candidate {
        Candidate {
            path: PathBuf::from(path),
            size_bytes,
            origin: CandidateOrigin::Filesystem {
                root: PathBuf::from("/media"),
            },
        }
    }

    #[test]
    fn single_size_and_name_agreement_is_probable() {
        let files = vec![torrent_file("Show S01/e01.mkv", 100)];
        let candidates = vec![candidate("/media/Show/e01.mkv", 100)];

        let plan = plan_matches(&files, &candidates);

        assert!(plan.is_complete());
        assert_eq!(plan.matched[0].confidence, MatchConfidence::Probable);
        assert!(plan.matched[0].evidence.name_matches);
    }

    #[test]
    fn size_agreement_alone_never_exceeds_ambiguous() {
        let files = vec![torrent_file("Show S01/e01.mkv", 100)];
        let candidates = vec![candidate("/media/Show/something-else.mkv", 100)];

        let plan = plan_matches(&files, &candidates);

        assert_eq!(plan.matched[0].confidence, MatchConfidence::Ambiguous);
        assert!(!plan.matched[0].evidence.name_matches);
        assert!(!plan.matched[0].evidence.piece_verified);
    }

    #[test]
    fn several_indistinguishable_candidates_are_left_unmatched() {
        let files = vec![torrent_file("Show S01/e01.mkv", 100)];
        let candidates = vec![
            candidate("/media/a/other.mkv", 100),
            candidate("/media/b/other.mkv", 100),
        ];

        let plan = plan_matches(&files, &candidates);

        assert!(!plan.is_complete());
        assert_eq!(
            plan.unmatched[0].reason,
            UnmatchedReason::Ambiguous { candidates: 2 }
        );
    }

    #[test]
    fn a_matching_name_breaks_a_size_tie() {
        let files = vec![torrent_file("Show S01/e01.mkv", 100)];
        let candidates = vec![
            candidate("/media/a/e01.mkv", 100),
            candidate("/media/b/other.mkv", 100),
        ];

        let plan = plan_matches(&files, &candidates);

        assert_eq!(plan.matched[0].source, PathBuf::from("/media/a/e01.mkv"));
        assert_eq!(plan.matched[0].confidence, MatchConfidence::Probable);
        assert_eq!(plan.matched[0].evidence.candidates_with_matching_size, 2);
    }

    #[test]
    fn wrong_size_is_no_candidate_at_all() {
        let files = vec![torrent_file("Show S01/e01.mkv", 100)];
        let candidates = vec![candidate("/media/Show/e01.mkv", 99)];

        let plan = plan_matches(&files, &candidates);

        assert_eq!(plan.unmatched[0].reason, UnmatchedReason::NoCandidate);
    }

    #[test]
    fn selection_is_deterministic_regardless_of_candidate_order() {
        let files = vec![torrent_file("Show S01/e01.mkv", 100)];
        let forward = vec![
            candidate("/media/a/e01.mkv", 100),
            candidate("/media/b/e01.mkv", 100),
        ];
        let reversed: Vec<Candidate> = forward.iter().rev().cloned().collect();

        // Two name-agreeing candidates are genuinely ambiguous either way.
        assert_eq!(
            plan_matches(&files, &forward),
            plan_matches(&files, &reversed)
        );
    }

    #[test]
    fn a_plan_is_only_as_strong_as_its_weakest_file() {
        let files = vec![
            torrent_file("Show S01/e01.mkv", 100),
            torrent_file("Show S01/e02.mkv", 200),
        ];
        let candidates = vec![
            candidate("/media/Show/e01.mkv", 100),
            candidate("/media/Show/mystery.mkv", 200),
        ];

        let plan = plan_matches(&files, &candidates);

        assert!(plan.is_complete());
        assert_eq!(plan.lowest_confidence(), Some(MatchConfidence::Ambiguous));
    }
}
