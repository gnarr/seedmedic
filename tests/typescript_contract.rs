//! The TypeScript unions still name every Rust variant.
//!
//! This is the structural half of the one real cost of
//! `docs/todos/0021-a-react-operator-ui.md`: four enums whose exhaustiveness is
//! load-bearing stop being exhaustively matched once they cross a language
//! boundary. Adding a `RepairState` used to fail the Rust build until every
//! `match` handled it; in TypeScript a missing case renders blank and ships.
//!
//! The other half lives in `web/src/format.ts`, where every `switch` over one of
//! these unions ends in `const exhaustive: never = value`. Together: adding a
//! variant in Rust fails **this** test until the union is updated, and then fails
//! `tsc` until the switch handles it.
//!
//! Deliberately a substring search rather than codegen, in the same spirit as
//! `web::tests::nothing_under_src_web_calls_expose`. Codegen would need a
//! dependency and a generated file nobody reads; this needs neither and fails
//! just as loudly.

use seedmedic::{
    library::MatchConfidence,
    repair::{RepairState, ReviewReason},
};

const API_TYPES: &str = include_str!("../web/src/api.ts");
const FORMAT: &str = include_str!("../web/src/format.ts");

/// Every wire value the client can receive for a state must be in its union.
#[test]
fn the_repair_state_union_names_every_variant() {
    let union = section(
        API_TYPES,
        "export const PROGRESSION",
        "export type MatchConfidence",
    );

    for state in RepairState::PROGRESSION
        .into_iter()
        .chain([RepairState::AwaitingReview, RepairState::Failed])
    {
        assert!(
            union.contains(&format!("\"{}\"", state.as_str())),
            "`{}` is a RepairState but is missing from web/src/api.ts's union — a \
             client that receives it would render an unstyled chip. Add it there, \
             then handle it in format.ts's `stateTone`.",
            state.as_str()
        );
    }
}

#[test]
fn the_match_confidence_union_names_every_variant() {
    for confidence in [
        MatchConfidence::Ambiguous,
        MatchConfidence::Probable,
        MatchConfidence::Operator,
        MatchConfidence::Exact,
    ] {
        let name = serde_json::to_value(confidence)
            .expect("serialises")
            .as_str()
            .expect("a string")
            .to_owned();
        assert!(
            API_TYPES.contains(&format!("\"{name}\"")),
            "`{name}` is a MatchConfidence missing from web/src/api.ts"
        );
        let body = section(
            FORMAT,
            "export function confidenceMeter",
            "/** Middle-truncate",
        );
        assert!(
            body.contains(&format!("case \"{name}\"")),
            "`{name}` is a MatchConfidence that web/src/format.ts's \
             `confidenceMeter` does not handle"
        );
    }
}

/// The 19 review reasons are *not* mirrored as a TypeScript union, on purpose:
/// the client never switches on one, it renders the `description` the server
/// sends. This test pins that arrangement, so adding a reason stays a Rust-only
/// change — and so nobody "helpfully" copies the list into TypeScript, where it
/// would immediately start drifting.
#[test]
fn review_reasons_are_never_mirrored_in_typescript() {
    let mirrored: Vec<&str> = [
        ReviewReason::AmbiguousMatch,
        ReviewReason::NoCandidates,
        ReviewReason::AutoResumeDisabled,
        ReviewReason::IncompleteData,
        ReviewReason::AliasedIncompleteData,
    ]
    .into_iter()
    .map(ReviewReason::as_str)
    .filter(|name| API_TYPES.contains(&format!("\"{name}\"")) || FORMAT.contains(name))
    .collect();

    assert!(
        mirrored.is_empty(),
        "these review reasons were copied into TypeScript: {mirrored:?}. The client \
         renders `review_reason_description` from the server instead, so a copy \
         here can only drift — see web/AGENTS.md."
    );
}

/// Every state has a branch in `stateTone`, so the client cannot receive one it
/// has no appearance for.
///
/// Scoped to `stateTone`'s own body, not the whole file. `stateGlyph` switches on
/// *tone* names, and five tones share a spelling with a state — so a whole-file
/// search passes on the wrong switch. That is exactly how the first version of
/// this test passed while `stateTone` was missing a branch.
#[test]
fn every_state_the_client_styles_has_a_tone_branch() {
    let body = section(
        FORMAT,
        "export function stateTone",
        "export function stateGlyph",
    );

    for state in RepairState::PROGRESSION
        .into_iter()
        .chain([RepairState::AwaitingReview, RepairState::Failed])
    {
        assert!(
            body.contains(&format!("case \"{}\"", state.as_str())),
            "web/src/format.ts's `stateTone` has no branch for `{}` — its `never` \
             assertion would throw at runtime rather than failing the build",
            state.as_str()
        );
    }
}

/// The text between two markers.
///
/// Scoping matters more than it looks: several `RepairState` names are also tone
/// names and several are also `MatchConfidence` names, so a whole-file search
/// finds them in the wrong switch and passes for the wrong reason.
fn section<'a>(source: &'a str, from: &str, to: &str) -> &'a str {
    let start = source
        .find(from)
        .unwrap_or_else(|| panic!("`{from}` not found in web/src/api.ts"));
    let end = source[start..]
        .find(to)
        .map(|offset| start + offset)
        .unwrap_or(source.len());
    &source[start..end]
}
