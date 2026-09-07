//! Where a GitHub API token comes from.
//!
//! GitHub allows 60 requests an hour per IP with none, and 5000 with one.
//! [`GITHUB_TOKEN_ENV`] wins when it is set. Otherwise Skillbase asks the
//! GitHub CLI, which most people who would hit the limit already have logged
//! in. The token is held in memory for the process and is never written down.
//!
//! Finder-launched applications do not inherit a shell environment, so the
//! CLI fallback is what makes the 5000-request budget reachable from the
//! installed `.app`. Extra well-known paths are searched because that launch
//! also has a thin `PATH`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use crate::github::GITHUB_TOKEN_ENV;

/// Which credential, if any, [`crate::GitHub::from_env`] will send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    /// [`GITHUB_TOKEN_ENV`] was set and not empty.
    Environment,
    /// `gh auth token` succeeded. The token is not stored on disk.
    GitHubCli,
    /// No token: 60 requests an hour per IP.
    None,
}

impl TokenSource {
    /// True when GitHub will treat the caller as authenticated.
    pub fn has_token(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// The token [`crate::GitHub::from_env`] will use, and where it came from.
///
/// Looked up once: an environment variable does not change under a running
/// process, and spawning `gh` on every request would be wasted work.
pub(crate) fn github_credential() -> (Option<String>, TokenSource) {
    github_credential_cached().clone()
}

/// Where the token came from, without handing the secret to the interface.
pub fn github_token_source() -> TokenSource {
    github_credential_cached().1
}

fn github_credential_cached() -> &'static (Option<String>, TokenSource) {
    static CREDENTIAL: OnceLock<(Option<String>, TokenSource)> = OnceLock::new();
    CREDENTIAL.get_or_init(lookup_github_token)
}

fn lookup_github_token() -> (Option<String>, TokenSource) {
    lookup_github_token_with(std::env::var(GITHUB_TOKEN_ENV).ok(), read_gh_cli_token)
}

fn lookup_github_token_with(
    env: Option<String>,
    gh: impl FnOnce() -> Option<String>,
) -> (Option<String>, TokenSource) {
    if let Some(token) = nonempty_token(env) {
        return (Some(token), TokenSource::Environment);
    }
    match nonempty_token(gh()) {
        Some(token) => (Some(token), TokenSource::GitHubCli),
        None => (None, TokenSource::None),
    }
}

fn nonempty_token(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn read_gh_cli_token() -> Option<String> {
    gh_binaries()
        .into_iter()
        .find_map(|binary| auth_token_from(&binary))
}

/// `PATH` first, then the places Homebrew and a manual install put `gh`
/// when the process was started from Finder.
fn gh_binaries() -> Vec<PathBuf> {
    let mut binaries = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            push_if_new(&mut binaries, dir.join("gh"));
        }
    }
    for extra in ["/opt/homebrew/bin/gh", "/usr/local/bin/gh"] {
        push_if_new(&mut binaries, PathBuf::from(extra));
    }
    if let Some(home) = std::env::var_os("HOME") {
        push_if_new(&mut binaries, PathBuf::from(home).join(".local/bin/gh"));
    }
    binaries
}

fn push_if_new(binaries: &mut Vec<PathBuf>, candidate: PathBuf) {
    if candidate.is_file() && !binaries.contains(&candidate) {
        binaries.push(candidate);
    }
}

fn auth_token_from(binary: &Path) -> Option<String> {
    let output = Command::new(binary)
        .args(["auth", "token", "--hostname", "github.com"])
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_gh_stdout(&String::from_utf8_lossy(&output.stdout))
}

/// First non-empty line, so a warning above the token still works. Rejects a
/// line with spaces, which is a message rather than a secret.
fn parse_gh_stdout(stdout: &str) -> Option<String> {
    let token = stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    if token.contains(' ') {
        None
    } else {
        Some(token.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_wins_over_the_cli() {
        let (token, source) =
            lookup_github_token_with(Some("env-token".into()), || Some("cli-token".into()));
        assert_eq!(token.as_deref(), Some("env-token"));
        assert_eq!(source, TokenSource::Environment);
    }

    #[test]
    fn a_blank_environment_variable_falls_through_to_the_cli() {
        let (token, source) =
            lookup_github_token_with(Some("  \n".into()), || Some("cli-token".into()));
        assert_eq!(token.as_deref(), Some("cli-token"));
        assert_eq!(source, TokenSource::GitHubCli);
    }

    #[test]
    fn a_missing_cli_is_anonymous() {
        let (token, source) = lookup_github_token_with(None, || None);
        assert_eq!(token, None);
        assert_eq!(source, TokenSource::None);
        assert!(!source.has_token());
    }

    #[test]
    fn gh_stdout_keeps_the_first_non_empty_line() {
        assert_eq!(
            parse_gh_stdout("\n  gho_exampleToken  \n").as_deref(),
            Some("gho_exampleToken")
        );
    }

    #[test]
    fn gh_stdout_that_is_a_sentence_is_not_a_token() {
        assert_eq!(parse_gh_stdout("no oauth token found for github.com"), None);
    }
}
