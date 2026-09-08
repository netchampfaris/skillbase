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
//!
//! The answer is held rather than looked up per request, but it is not frozen:
//! a user who runs `gh auth login` after the application started can ask for it
//! to be read again with [`refresh_github_token`].

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{PoisonError, RwLock};

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

/// What the last lookup found, or `None` before the first one.
///
/// A lock rather than a `OnceLock` because the answer can change under a
/// running process: `gh auth login` in another window is exactly the case
/// [`refresh_github_token`] exists for.
static CREDENTIAL: RwLock<Option<(Option<String>, TokenSource)>> = RwLock::new(None);

/// The token [`crate::GitHub::from_env`] will use, and where it came from.
///
/// Looked up on the first call and then held, because spawning `gh` on every
/// request would be wasted work. [`refresh_github_token`] is what replaces it.
pub(crate) fn github_credential() -> (Option<String>, TokenSource) {
    github_credential_cached()
}

/// Where the token came from, without handing the secret to the interface.
pub fn github_token_source() -> TokenSource {
    github_credential_cached().1
}

/// Read the environment and `gh` again, and adopt what they say now.
///
/// For the user who hits a private-repository 404, runs `gh auth login`, and
/// comes back: without this the process would go on sending whatever it found
/// at startup until it was relaunched.
///
/// Blocking. It stats every entry on `PATH` and waits on a `gh` subprocess that
/// may in turn wait on the keychain, so callers run it on a background thread.
pub fn refresh_github_token() -> TokenSource {
    refresh_with(lookup_github_token)
}

/// [`refresh_github_token`] with the lookup passed in, so a test can drive the
/// replacement without running `gh`.
fn refresh_with(lookup: impl FnOnce() -> (Option<String>, TokenSource)) -> TokenSource {
    let found = lookup();
    let source = found.1;
    *CREDENTIAL.write().unwrap_or_else(PoisonError::into_inner) = Some(found);
    source
}

fn github_credential_cached() -> (Option<String>, TokenSource) {
    credential_cached_with(lookup_github_token)
}

/// [`github_credential_cached`] with the lookup passed in. Same reason as
/// [`refresh_with`]: the holding is what is worth testing, and the lookup is
/// the part that spawns a subprocess.
fn credential_cached_with(
    lookup: impl FnOnce() -> (Option<String>, TokenSource),
) -> (Option<String>, TokenSource) {
    if let Some(found) = CREDENTIAL
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
    {
        return found;
    }
    // Looked up outside the write lock, so a second caller arriving during a
    // slow `gh` is not blocked behind it. Both then store, and both store the
    // same answer.
    let found = lookup();
    let mut slot = CREDENTIAL.write().unwrap_or_else(PoisonError::into_inner);
    slot.get_or_insert(found).clone()
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

    /// The point of the lock: the first call looks up and holds the answer, and
    /// [`refresh_github_token`] replaces what is held, so a `gh auth login` that
    /// happens while the process runs is reachable without a relaunch.
    ///
    /// Drives the two functions the interface calls, with only the `gh`
    /// subprocess substituted.
    #[test]
    fn a_refresh_replaces_the_answer_the_first_call_held() {
        let saved = CREDENTIAL
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .take();

        // Nothing held yet, so the first call looks up.
        let (token, source) = credential_cached_with(|| (None, TokenSource::None));
        assert_eq!(token, None);
        assert_eq!(source, TokenSource::None);
        // And the second does not: this is what stops `gh` being spawned on
        // every request.
        assert_eq!(
            credential_cached_with(|| panic!("the held answer was looked up again")).1,
            TokenSource::None
        );

        // The login happens now.
        assert_eq!(
            refresh_with(|| (Some("gho_exampleToken".into()), TokenSource::GitHubCli)),
            TokenSource::GitHubCli
        );
        assert_eq!(github_token_source(), TokenSource::GitHubCli);
        assert_eq!(github_credential().0.as_deref(), Some("gho_exampleToken"));

        *CREDENTIAL.write().unwrap_or_else(PoisonError::into_inner) = saved;
    }
}
