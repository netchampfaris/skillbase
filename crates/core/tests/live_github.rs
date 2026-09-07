//! End-to-end checks against the real GitHub API.
//!
//! Every test here is `#[ignore]`, so `cargo test` does not run them and the
//! ordinary suite stays offline. Run them deliberately:
//!
//! ```sh
//! cargo test -p skillbase-core --test live_github -- --ignored --nocapture
//! ```
//!
//! They exist because the unit tests prove the parsing against payloads we
//! wrote ourselves, which cannot catch GitHub changing a field name. These
//! read public repositories only, install into a temporary directory, and
//! spend about four requests of the 60-an-hour unauthenticated budget.

use skillbase_core::{
    GitHub, InstallOptions, Installer, RemoteCache, Roots, SkillLocation, UpdateStatus, UreqHttp,
    install_from_github, update_targets,
};

/// A repository that is stable, public, and the reference implementation of
/// the format.
const LOCATION: &str = "anthropics/skills/skills/pdf";

#[test]
#[ignore = "makes real network requests"]
fn resolves_and_installs_a_skill_from_github() {
    let home = tempfile::tempdir().expect("temp home");
    let roots = Roots::new(home.path());
    let github = GitHub::from_env(UreqHttp::new());

    let location = SkillLocation::parse(LOCATION).expect("parse the location");
    eprintln!("location = {location:?}");

    let installer = Installer::new(roots.clone());
    let mut cache = RemoteCache::read(&roots);
    let installed = install_from_github(
        &installer,
        &github,
        &location,
        &InstallOptions::new(),
        &mut cache,
    )
    .expect("install from github");

    eprintln!(
        "installed {} into {} ({} files)",
        installed.name,
        installed.dir.display(),
        installed.files
    );

    // The bytes landed in the store, which is `~/.agents/skills` under the
    // temporary home rather than the real one.
    assert!(installed.dir.starts_with(roots.store_dir()));
    assert!(installed.dir.join("SKILL.md").is_file());

    // Provenance is written into the file itself, the four keys `gh skill
    // install` uses.
    let text = std::fs::read_to_string(installed.dir.join("SKILL.md")).expect("read SKILL.md");
    for key in [
        "github-repo",
        "github-ref",
        "github-tree-sha",
        "github-path",
    ] {
        assert!(text.contains(key), "SKILL.md is missing {key}:\n{text}");
    }

    let sha = installed
        .provenance
        .tree_sha
        .as_deref()
        .expect("a tree sha was recorded");
    assert_eq!(sha.len(), 40, "a tree sha is 40 hex characters: {sha}");
    eprintln!("tree sha = {sha}");
}

#[test]
#[ignore = "makes real network requests"]
fn a_freshly_installed_skill_reports_no_update() {
    let home = tempfile::tempdir().expect("temp home");
    let roots = Roots::new(home.path());
    let github = GitHub::from_env(UreqHttp::new());

    let location = SkillLocation::parse(LOCATION).expect("parse the location");
    let installer = Installer::new(roots.clone());
    let mut cache = RemoteCache::read(&roots);
    let installed = install_from_github(
        &installer,
        &github,
        &location,
        &InstallOptions::new(),
        &mut cache,
    )
    .expect("install from github");

    // Rediscover it the way the application does, rather than constructing a
    // target by hand.
    let found = skillbase_core::Discovery::new(roots.clone()).run();
    let lock = skillbase_core::SkillLock::read(&roots);
    let targets = update_targets(&found.skills, &lock);
    assert!(
        targets.iter().any(|t| t.name == installed.name),
        "the installed skill should be an update target"
    );

    let report = skillbase_core::check_updates(&github, &targets, &mut cache);
    eprintln!(
        "checked {} repos in {} requests",
        report.repos_checked(),
        report.requests()
    );

    match report.status(&installed.name) {
        Some(UpdateStatus::UpToDate) => {}
        other => panic!("just installed, so it should be up to date, got {other:?}"),
    }
}
