use maud::{DOCTYPE, Markup, html};

use crate::repair::RepairState;

/// Page furniture that depends on process state rather than the page being
/// rendered — currently just the unconfigured-settings banner. `Chrome::none`
/// for the error page, which has no `AppState` to build one from; the real
/// thing everywhere else.
#[derive(Clone, Default)]
pub struct Chrome {
    /// The configuration file SeedMedic read, for the banner to name.
    config_path: String,
    /// Every unmet-setting warning `Config::problems()` found, verbatim.
    /// Empty once a deployment has nothing left to configure — that is what
    /// keeps the banner absent from a fully-configured instance's pages.
    warnings: Vec<String>,
}

impl Chrome {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn new(config_path: String, warnings: Vec<String>) -> Self {
        Self {
            config_path,
            warnings,
        }
    }
}

/// Page shell. One stylesheet, inline, because a self-hosted operator UI does
/// not need an asset pipeline.
pub fn page(chrome: &Chrome, title: &str, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "SeedMedic — " (title) }
                style { (STYLE) }
            }
            body {
                header {
                    a href="/" { h1 { "SeedMedic" } }
                    p.tagline { "Hit-and-run repair" }
                }
                @if !chrome.warnings.is_empty() {
                    div.notice.setup {
                        strong { "Not fully configured (" (chrome.config_path) "):" }
                        ul {
                            @for warning in &chrome.warnings {
                                li { (warning) }
                            }
                        }
                    }
                }
                main { (body) }
            }
        }
    }
}

/// Colour-coded state chip. Review and failure must be obvious at a glance.
pub fn state_chip(state: RepairState) -> Markup {
    let class = match state {
        RepairState::Completed => "chip done",
        RepairState::AwaitingReview => "chip review",
        RepairState::Failed => "chip failed",
        RepairState::Seeding => "chip seeding",
        _ => "chip",
    };
    html! { span class=(class) { (state.as_str()) } }
}

const STYLE: &str = r#"
:root { color-scheme: light dark; --fg: #1a1a1a; --muted: #666; --line: #d8d8d8; --bg: #fdfdfd; --accent: #2b6cb0; }
@media (prefers-color-scheme: dark) {
  :root { --fg: #e8e8e8; --muted: #9a9a9a; --line: #333; --bg: #161616; --accent: #7aa7d9; }
}
* { box-sizing: border-box; }
body { margin: 0; padding: 0 1.5rem 4rem; font: 15px/1.5 system-ui, sans-serif; color: var(--fg); background: var(--bg); }
header { display: flex; align-items: baseline; gap: .75rem; padding: 1.5rem 0 1rem; border-bottom: 1px solid var(--line); margin-bottom: 1.5rem; }
header a { text-decoration: none; color: inherit; }
h1 { font-size: 1.25rem; margin: 0; }
h2 { font-size: 1rem; margin: 2rem 0 .5rem; }
.tagline { color: var(--muted); margin: 0; font-size: .85rem; }
main { max-width: 60rem; margin: 0 auto; }
table { width: 100%; border-collapse: collapse; font-size: .9rem; }
th { text-align: left; color: var(--muted); font-weight: 500; }
th, td { padding: .5rem .6rem; border-bottom: 1px solid var(--line); vertical-align: top; }
td.wrap { word-break: break-all; }
a { color: var(--accent); }
.chip { display: inline-block; padding: .1rem .5rem; border-radius: 999px; border: 1px solid var(--line); font-size: .78rem; white-space: nowrap; }
.chip.done { border-color: #2f855a; color: #2f855a; }
.chip.review { border-color: #b7791f; color: #b7791f; }
.chip.failed { border-color: #c53030; color: #c53030; }
.chip.seeding { border-color: #2b6cb0; color: #2b6cb0; }
.notice { border: 1px solid #b7791f; border-left-width: 4px; padding: .75rem 1rem; margin: 1rem 0; }
.notice.danger { border-color: #c53030; }
.actions { display: flex; gap: .5rem; margin: 1rem 0; flex-wrap: wrap; }
button { font: inherit; padding: .4rem .9rem; border: 1px solid var(--line); background: transparent; color: var(--fg); border-radius: 4px; cursor: pointer; }
button.danger { border-color: #c53030; color: #c53030; }
dl { display: grid; grid-template-columns: max-content 1fr; gap: .3rem 1rem; margin: 0; }
dt { color: var(--muted); }
dd { margin: 0; word-break: break-all; }
pre { background: rgba(128,128,128,.1); padding: .5rem; border-radius: 4px; overflow-x: auto; font-size: .8rem; margin: 0; }
.empty { color: var(--muted); padding: 2rem 0; }
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fully_configured_instance_shows_no_banner() {
        let markup = page(&Chrome::none(), "Status", html! { p { "body" } }).into_string();
        assert!(!markup.contains("Not fully configured"));
    }

    #[test]
    fn an_unmet_setting_shows_in_the_banner() {
        let chrome = Chrome::new(
            "/etc/seedmedic/config.toml".to_owned(),
            vec!["staging.root is unset".to_owned()],
        );
        let markup = page(&chrome, "Status", html! { p { "body" } }).into_string();

        assert!(markup.contains("Not fully configured"));
        assert!(markup.contains("/etc/seedmedic/config.toml"));
        assert!(markup.contains("staging.root is unset"));
    }
}
