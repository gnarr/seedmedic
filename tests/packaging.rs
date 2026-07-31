//! The container layout, asserted over the shipped files.
//!
//! Nothing else in the repository tests the image or the compose file, and a
//! container harness would be out of all proportion to twenty lines of shell.
//! What these cover instead are the invariants that fail *silently*: a mount
//! that resolves to the wrong path, defaults that would refuse to start, and
//! one arithmetic coincidence the whole layout rests on.
//!
//! See docs/todos/0020-a-container-that-just-runs.md.

use std::path::{Path, PathBuf};

use seedmedic::{config::Config, staging::StagingRoot};

const COMPOSE: &str = include_str!("../docker-compose.yml");
const ENV_EXAMPLE: &str = include_str!("../.env.example");
const DOCKERFILE: &str = include_str!("../Dockerfile");

/// Every `${NAME:-default}` or `${NAME}` in the compose file, ignoring comment
/// lines — a `${VAR}` inside a `#` comment is never interpolated by Compose,
/// and several of the comments show example values on purpose.
fn interpolated_variables() -> Vec<String> {
    let mut names = Vec::new();
    for line in COMPOSE.lines() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        let mut rest = line;
        while let Some(start) = rest.find("${") {
            rest = &rest[start + 2..];
            let end = rest
                .find(['}', ':'])
                .expect("an opened ${ must be closed in a valid compose file");
            let name = rest[..end].to_owned();
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

/// The value assigned to `key` in `.env.example`, including commented-out
/// assignments — those document a variable that has a default elsewhere, which
/// still counts as defined.
fn env_example_value(key: &str) -> Option<String> {
    ENV_EXAMPLE.lines().find_map(|line| {
        let line = line.trim().trim_start_matches('#').trim();
        line.strip_prefix(key)?
            .strip_prefix('=')
            .map(|value| value.trim().to_owned())
    })
}

/// A `KEY value` directive from the Dockerfile's runtime stage.
/// A `KEY value` directive from the Dockerfile's **runtime** stage — the text
/// after the last `FROM`.
///
/// Scoped deliberately. This used to take the last matching line in the whole
/// file, which was correct only by accident of stage ordering: the builder stages
/// have their own `WORKDIR`s, and adding one *after* the runtime stage would have
/// silently pointed
/// `the_container_puts_the_config_and_the_database_in_one_directory` at the wrong
/// one. 0021 added a Node stage, which is exactly the change that would have
/// tripped it.
fn dockerfile_directive(keyword: &str) -> Option<String> {
    let runtime = DOCKERFILE
        .rsplit_once("\nFROM ")
        .map_or(DOCKERFILE, |(_, tail)| tail);
    runtime
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix(keyword)?;
            Some(rest.trim().to_owned())
        })
        .next_back()
}

#[test]
fn every_variable_the_compose_file_interpolates_is_documented_in_env_example() {
    for name in interpolated_variables() {
        assert!(
            env_example_value(&name).is_some(),
            "docker-compose.yml interpolates ${{{name}}} but .env.example never mentions it; \
             an undefined variable resolves to an empty string, which silently corrupts a \
             mount spec or a port binding rather than failing"
        );
    }
}

#[test]
fn the_staging_mount_is_the_same_path_on_both_sides() {
    let mount = COMPOSE
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("- ${STAGING_PATH"))
        .expect("docker-compose.yml must mount the staging path");
    let (host, container) = mount
        .trim_start_matches("- ")
        .split_once("}:")
        .expect("the staging mount must have a host and a container side");

    assert_eq!(
        format!("{host}}}"),
        container,
        "SeedMedic hands the staging root to the download client verbatim — there is no path \
         mapping outside [[arr]] — so the host and container sides of this mount must be the \
         same string, or qBittorrent's recheck finds 0% after the staging work is already done"
    );
}

#[test]
fn the_example_staging_and_media_paths_do_not_overlap() {
    let staging = env_example_value("STAGING_PATH").expect("STAGING_PATH");
    let media = env_example_value("MEDIA_PATH").expect("MEDIA_PATH");

    StagingRoot::check_overlap(Path::new(&staging), &[PathBuf::from(&media)]).expect(
        "the shipped defaults must not overlap: a staging root inside a library root is a \
         hard startup error, so shipping one would make every new install fail to start",
    );
}

#[test]
fn the_container_puts_the_config_and_the_database_in_one_directory() {
    let workdir = dockerfile_directive("WORKDIR").expect("the Dockerfile must set a WORKDIR");
    let config_path = dockerfile_directive("ENV SEEDMEDIC_CONFIG=")
        .expect("the Dockerfile must set SEEDMEDIC_CONFIG");

    // `database.path` defaults to the *relative* "data/seedmedic.db", resolved
    // against the process working directory. `WORKDIR /` is what lands it in
    // /data beside the config, with no Rust change and no edit to
    // config.example.toml. That coincidence is invisible and load-bearing:
    // change either end and the container splits its config and its database
    // across two directories, only one of which is mounted.
    let database = Path::new(&workdir).join(Config::default().database.path);

    assert_eq!(
        database.parent(),
        Path::new(&config_path).parent(),
        "the container's database ({}) and config ({config_path}) must share one directory, \
         because docker-compose.yml mounts exactly one",
        database.display()
    );
}

#[test]
fn the_dockerfile_declares_no_volumes() {
    assert!(
        dockerfile_directive("VOLUME").is_none(),
        "VOLUME makes a bare `docker run` mint a root-owned anonymous volume that the \
         settings page can read but never write — the case \
         docs/todos/0017-the-settings-pages.md had to detect and refuse. The entrypoint \
         fixes ownership at run time instead."
    );
}

#[test]
fn the_entrypoint_never_recurses_into_the_staging_directory() {
    let entrypoint = include_str!("../docker/entrypoint.sh");
    let staging_chown = entrypoint
        .lines()
        .find(|line| line.trim().starts_with("own \"$STAGING_DIR\""))
        .expect("the entrypoint must take ownership of the staging directory");

    assert!(
        staging_chown.trim().ends_with(r#"own "$STAGING_DIR" """#),
        "the staging chown must pass an empty recursion flag. A staged file materialised by \
         hard link IS the library file, so `chown -R` here would rewrite ownership inside the \
         media library — which AGENTS.md's first rule forbids outright. Found: {staging_chown}"
    );
}

/// Without this the image ships the `.gitkeep`-only bundle and every page serves
/// the "UI was not built" notice — silent, and only visible in production, which
/// is exactly the class of failure this file exists to catch.
#[test]
fn the_builder_receives_a_bundle_built_by_the_node_stage() {
    assert!(
        DOCKERFILE.contains("--from=web /web/dist"),
        "the Rust builder must copy the operator UI from the node stage"
    );
    assert!(
        DOCKERFILE.contains("npm ci"),
        "the node stage must install from the lockfile, not resolve a fresh tree"
    );
}

/// The runtime stage's whole point: three files and no package manager.
///
/// 0021 added a build stage, which is the moment somebody is most likely to
/// "simplify" by installing something into the final image.
#[test]
fn the_runtime_stage_still_copies_three_things_and_installs_nothing() {
    let runtime = DOCKERFILE
        .rsplit_once("\nFROM ")
        .map(|(_, tail)| tail)
        .expect("a runtime stage");

    let copies = runtime
        .lines()
        .filter(|line| line.trim_start().starts_with("COPY "))
        .count();
    assert_eq!(copies, 3, "the runtime stage gained a COPY: {runtime}");
    assert!(
        !runtime.contains("apt-get"),
        "the runtime image installs no packages, deliberately — reqwest is rustls \
         and the health check is bash /dev/tcp precisely so it does not have to"
    );
}
