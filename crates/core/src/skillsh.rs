//! Searching skills.sh, for discovery only.
//!
//! # Why the undocumented endpoint
//!
//! skills.sh publishes an `/api/v1/` that needs a Vercel OIDC token, which a
//! desktop application cannot obtain. The endpoint used here,
//! `GET /api/search`, is the one the official `npx skills` CLI itself calls. It
//! needs no authentication and no particular `User-Agent`. **It is
//! undocumented and may change or disappear without notice.**
//!
//! That is exactly why nothing but search goes through it. `/api/download` is
//! not used, and neither installing nor update checking touches skills.sh at
//! all: both go to GitHub. If this endpoint changes, search stops working and
//! everything else keeps working.
//!
//! # What a search result does and does not carry
//!
//! A hit is `{id, skillId, name, installs, source}`, where `source` is
//! `"owner/repo"`. There is no path, no ref and no sha, so a hit is not yet
//! something that can be installed. [`resolve`] turns one into real
//! [`SkillLocation`]s by walking the repository for a directory named `skillId`
//! that holds a `SKILL.md`, which is also how it copes with a skill sitting at
//! the repository root and with a repository holding the same skill id twice.
//!
//! A hit carries no description either, so two results called `pdf` say
//! nothing that tells them apart beyond the repository they come from.
//! [`describe`] is the answer to that: it settles where the skill is and reads
//! the `description` out of its own `SKILL.md`. It costs three requests against
//! GitHub's hourly budget, so it is called for one hit at a time, when
//! something asks about that hit, and never for a whole page of results.
//!
//! The query must be at least [`MIN_QUERY_LEN`] characters and `limit` is
//! capped at [`MAX_LIMIT`]. `owner` narrows a search; category and tag
//! parameters are accepted by the server and silently ignored, so this module
//! does not offer them. There is no pagination beyond the limit.
//!
//! Every call blocks. Run them on a background task.

use serde::Deserialize;
use thiserror::Error;

use crate::doc::SkillDoc;
use crate::github::{GitHub, GitHubError, RepoRef, SkillLocation};
use crate::http::{Http, HttpError};
use crate::provenance::split_owner_repo;

/// The search endpoint. Undocumented, and used by the official CLI.
pub const SEARCH_URL: &str = "https://skills.sh/api/search";

/// Shortest query the server accepts. A shorter one returns nothing useful, so
/// it is refused here rather than sent.
pub const MIN_QUERY_LEN: usize = 2;

/// Largest `limit` the server honours.
pub const MAX_LIMIT: usize = 200;

/// The default number of hits asked for.
pub const DEFAULT_LIMIT: usize = 50;

/// Anything that can go wrong searching.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SearchError {
    /// The request never reached the server.
    #[error(transparent)]
    Http(#[from] HttpError),

    /// The query is shorter than [`MIN_QUERY_LEN`].
    #[error("a search needs at least {MIN_QUERY_LEN} characters")]
    QueryTooShort,

    /// The server answered with something other than success.
    #[error("skills.sh answered {status}")]
    Status {
        /// The status code.
        status: u16,
    },

    /// The response was not the shape this module expects, which is what a
    /// change to an undocumented endpoint looks like.
    #[error("skills.sh returned an unexpected response: {detail}")]
    Malformed {
        /// What was wrong with it.
        detail: String,
    },
}

/// One skill in a search result.
///
/// Carries no path, ref or sha: [`resolve`] is what turns it into somewhere a
/// skill can actually be fetched from.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SearchHit {
    /// The registry's own identifier for the row.
    #[serde(default)]
    pub id: String,
    /// The skill's id, which is the directory name to look for in the
    /// repository.
    #[serde(rename = "skillId", default)]
    pub skill_id: String,
    /// The skill's display name.
    #[serde(default)]
    pub name: String,
    /// How many times the registry has seen it installed.
    #[serde(default)]
    pub installs: u64,
    /// The repository, as `owner/repo`.
    #[serde(default)]
    pub source: String,
}

impl SearchHit {
    /// The `owner` and `repo` of [`SearchHit::source`].
    pub fn owner_repo(&self) -> Option<(String, String)> {
        split_owner_repo(&self.source)
    }

    /// The repository's web URL, when the source names one.
    pub fn repo_url(&self) -> Option<String> {
        let (owner, repo) = self.owner_repo()?;
        Some(format!("https://github.com/{owner}/{repo}"))
    }
}

/// A whole search response.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct SearchResults {
    /// The query, echoed by the server.
    #[serde(default)]
    pub query: String,
    /// How many hits the server says it found.
    #[serde(default)]
    pub count: usize,
    /// The hits, in the order the server returned them.
    #[serde(default)]
    pub skills: Vec<SearchHit>,
}

impl SearchResults {
    /// True when nothing matched.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}

/// The skills.sh search client.
#[derive(Debug)]
pub struct SkillsSh<H: Http> {
    http: H,
    search_url: String,
}

impl<H: Http> SkillsSh<H> {
    /// A client pointed at the real endpoint.
    pub fn new(http: H) -> Self {
        Self {
            http,
            search_url: SEARCH_URL.to_string(),
        }
    }

    /// Points the client at another endpoint. For tests.
    pub fn with_search_url(mut self, url: impl Into<String>) -> Self {
        self.search_url = url.into();
        self
    }

    /// The underlying transport.
    pub fn http(&self) -> &H {
        &self.http
    }

    /// Searches for `query`.
    ///
    /// `limit` is clamped to `1..=`[`MAX_LIMIT`]. `owner` narrows the search to
    /// one account when given. A query shorter than [`MIN_QUERY_LEN`] is
    /// refused without a request.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
        owner: Option<&str>,
    ) -> Result<SearchResults, SearchError> {
        let query = query.trim();
        if query.chars().count() < MIN_QUERY_LEN {
            return Err(SearchError::QueryTooShort);
        }
        let limit = limit.clamp(1, MAX_LIMIT);
        let mut url = format!(
            "{}?q={}&limit={limit}",
            self.search_url,
            percent_encode(query)
        );
        if let Some(owner) = owner.map(str::trim).filter(|o| !o.is_empty()) {
            url.push_str(&format!("&owner={}", percent_encode(owner)));
        }

        let response = self.http.get(&url, &[("Accept", "application/json")])?;
        if !response.is_success() {
            return Err(SearchError::Status {
                status: response.status,
            });
        }
        serde_json::from_slice(&response.body).map_err(|e| SearchError::Malformed {
            detail: e.to_string(),
        })
    }
}

/// Turns a search hit into the places it could actually be fetched from.
///
/// A hit names a repository and a skill id and nothing else, so the repository
/// has to be looked at: one request for the default branch, one for the ref,
/// and one recursive tree, then every directory named `skill_id` that holds a
/// `SKILL.md` comes back as a [`SkillLocation`]. The repository root counts
/// when the repository is itself the skill.
///
/// `reference` pins a branch or tag; without one the repository's default
/// branch is used, because a search result records no ref and guessing `main`
/// is wrong for every repository still on `master`.
///
/// More than one match is returned rather than picked between: two directories
/// named `pdf` in one repository are a real thing, and only the user knows
/// which was meant. An empty result means the registry and the repository
/// disagree, usually because the skill was renamed or removed upstream.
pub fn resolve<H: Http>(
    gh: &GitHub<H>,
    hit: &SearchHit,
    reference: Option<&str>,
) -> Result<Vec<SkillLocation>, GitHubError> {
    let (repo, _, paths) = locate(gh, hit, reference)?;
    Ok(paths
        .into_iter()
        .map(|path| SkillLocation::new(repo.clone(), path))
        .collect())
}

/// What a search result actually is, read from the repository it names.
///
/// The registry answers with a name, an owner and an install count, none of
/// which says what the skill does. This is the rest of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitDetail {
    /// The skill's directory within the repository. Empty at the root.
    pub path: String,
    /// The branch or tag it was read from.
    pub reference: String,
    /// The `description` from its `SKILL.md`, when the file has one.
    pub description: Option<String>,
}

/// Reads what a search result says about itself.
///
/// Settles where the skill is the same way [`resolve`] does, then reads the
/// first directory's `SKILL.md` from `raw.githubusercontent.com`. `Ok(None)`
/// means the repository holds no such directory, which is the same disagreement
/// between registry and repository that an empty [`resolve`] reports.
///
/// A hit that resolves to more than one directory is described by the first of
/// them, in tree order. Two directories of the same name in one repository is
/// rare, and one description is closer to an answer than none.
///
/// Three requests against GitHub's hourly budget: the default branch, the ref,
/// and the tree. Reading the file itself costs nothing, because
/// `raw.githubusercontent.com` is not on that budget.
///
/// Blocking, and it makes network requests. Run it on a background task.
pub fn describe<H: Http>(
    gh: &GitHub<H>,
    hit: &SearchHit,
) -> Result<Option<HitDetail>, GitHubError> {
    let (repo, commit, paths) = locate(gh, hit, None)?;
    let Some(path) = paths.into_iter().next() else {
        return Ok(None);
    };
    let file = if path.is_empty() {
        "SKILL.md".to_string()
    } else {
        format!("{path}/SKILL.md")
    };
    // Anything the file turns out not to be is the same answer as no
    // description: bytes that are not UTF-8, a document with no frontmatter, a
    // frontmatter with no `description`. The path and the branch are worth
    // handing back either way, because they are what tells two rows apart when
    // neither has a description.
    let description = gh
        .fetch_file(&repo, &commit, &file)?
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|text| SkillDoc::parse(&text).ok())
        .and_then(|doc| doc.frontmatter.description().map(str::to_string))
        .map(|description| description.trim().to_string())
        .filter(|description| !description.is_empty());

    Ok(Some(HitDetail {
        path,
        reference: repo.reference,
        description,
    }))
}

/// The repository, the commit its ref points at, and every directory in it that
/// answers to the hit's skill id.
///
/// Shared by [`resolve`] and [`describe`] so the two cannot come to disagree
/// about where a search result points.
fn locate<H: Http>(
    gh: &GitHub<H>,
    hit: &SearchHit,
    reference: Option<&str>,
) -> Result<(RepoRef, String, Vec<String>), GitHubError> {
    let (owner, repo_name) = hit.owner_repo().ok_or_else(|| GitHubError::NotFound {
        what: format!("a repository named `{}`", hit.source),
    })?;
    let reference = match reference {
        Some(reference) => reference.to_string(),
        None => gh.default_branch(&owner, &repo_name)?,
    };
    let repo = RepoRef::new(owner, repo_name, reference);
    let commit = gh.ref_sha(&repo)?;
    let skill_id = if hit.skill_id.is_empty() {
        hit.name.as_str()
    } else {
        hit.skill_id.as_str()
    };
    let paths = gh.find_skill_dirs(&repo, &commit, skill_id)?;
    Ok((repo, commit, paths))
}

/// Percent-encodes a query parameter value.
///
/// Only unreserved characters are left alone, which is more conservative than
/// necessary and never wrong.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::HttpResponse;
    use crate::http::fake::FakeHttp;

    /// The shape the endpoint really returns.
    const PAYLOAD: &str = r#"{
      "query": "pdf",
      "count": 3,
      "skills": [
        {"id":"cm7","skillId":"pdf","name":"PDF Processing","installs":1842,"source":"anthropics/skills"},
        {"id":"cm8","skillId":"pdf-forms","name":"PDF Forms","installs":97,"source":"someone/agent-skills"},
        {"id":"cm9","skillId":"pdfplumber","name":"pdfplumber","installs":4,"source":"third/party"}
      ]
    }"#;

    fn client(http: FakeHttp) -> SkillsSh<FakeHttp> {
        SkillsSh::new(http).with_search_url("https://skills.test/api/search")
    }

    #[test]
    fn a_real_shaped_payload_parses() {
        let http = FakeHttp::new();
        http.json("https://skills.test/api/search?q=pdf&limit=50", PAYLOAD);
        let results = client(http).search("pdf", DEFAULT_LIMIT, None).unwrap();

        assert_eq!(results.query, "pdf");
        assert_eq!(results.count, 3);
        assert_eq!(results.skills.len(), 3);

        let first = &results.skills[0];
        assert_eq!(first.skill_id, "pdf");
        assert_eq!(first.name, "PDF Processing");
        assert_eq!(first.installs, 1842);
        assert_eq!(first.source, "anthropics/skills");
        assert_eq!(
            first.owner_repo(),
            Some(("anthropics".to_string(), "skills".to_string()))
        );
        assert_eq!(
            first.repo_url().as_deref(),
            Some("https://github.com/anthropics/skills")
        );
    }

    #[test]
    fn the_query_is_encoded_the_limit_clamped_and_the_owner_optional() {
        let http = FakeHttp::new();
        http.pattern(
            "skills.test",
            HttpResponse::new(200, PAYLOAD.as_bytes().to_vec()),
        );
        let client = client(http);

        client
            .search("pdf forms", 5000, Some("anthropics"))
            .unwrap();
        client.search("  pdf  ", 0, Some("   ")).unwrap();

        assert_eq!(
            client.http().urls(),
            [
                "https://skills.test/api/search?q=pdf%20forms&limit=200&owner=anthropics",
                "https://skills.test/api/search?q=pdf&limit=1",
            ]
        );
    }

    #[test]
    fn a_short_query_is_refused_without_a_request() {
        let http = FakeHttp::new();
        let client = client(http);
        assert!(matches!(
            client.search("p", 10, None),
            Err(SearchError::QueryTooShort)
        ));
        assert_eq!(client.http().request_count(), 0);
    }

    #[test]
    fn a_changed_endpoint_is_reported_rather_than_guessed_at() {
        let http = FakeHttp::new();
        http.reply(
            "https://skills.test/api/search?q=pdf&limit=50",
            HttpResponse::new(200, b"<!doctype html>".to_vec()),
        );
        let err = client(http).search("pdf", DEFAULT_LIMIT, None).unwrap_err();
        assert!(matches!(err, SearchError::Malformed { .. }), "{err:?}");

        let http = FakeHttp::new();
        http.reply(
            "https://skills.test/api/search?q=pdf&limit=50",
            HttpResponse::new(503, Vec::new()),
        );
        assert!(matches!(
            client(http).search("pdf", DEFAULT_LIMIT, None),
            Err(SearchError::Status { status: 503 })
        ));
    }

    #[test]
    fn resolving_a_hit_finds_every_matching_directory() {
        let http = FakeHttp::new();
        http.json(
            "https://api.test/repos/o/r",
            r#"{"default_branch":"trunk"}"#,
        );
        http.json(
            "https://api.test/repos/o/r/git/ref/heads/trunk",
            r#"{"object":{"sha":"c1","type":"commit"}}"#,
        );
        http.json(
            "https://api.test/repos/o/r/git/trees/c1?recursive=1",
            r#"{"sha":"root","truncated":false,"tree":[
                {"path":"a/pdf/SKILL.md","type":"blob","sha":"b1"},
                {"path":"b/pdf/SKILL.md","type":"blob","sha":"b2"},
                {"path":"b/other/SKILL.md","type":"blob","sha":"b3"}]}"#,
        );
        let gh = GitHub::new(http).with_endpoints(
            "https://api.test",
            "https://codeload.test",
            "https://raw.test",
        );

        let hit = SearchHit {
            id: "cm7".into(),
            skill_id: "pdf".into(),
            name: "PDF".into(),
            installs: 1,
            source: "o/r".into(),
        };
        let found = resolve(&gh, &hit, None).unwrap();
        assert_eq!(
            found
                .iter()
                .map(|location| location.path.as_str())
                .collect::<Vec<_>>(),
            ["a/pdf", "b/pdf"]
        );
        assert!(found.iter().all(|l| l.repo.reference == "trunk"));
    }

    #[test]
    fn resolving_a_hit_at_the_repository_root_works_too() {
        let http = FakeHttp::new();
        http.json(
            "https://api.test/repos/o/pdf/git/ref/heads/main",
            r#"{"object":{"sha":"c1","type":"commit"}}"#,
        );
        http.json(
            "https://api.test/repos/o/pdf/git/trees/c1?recursive=1",
            r#"{"sha":"root","truncated":false,"tree":[
                {"path":"SKILL.md","type":"blob","sha":"b1"}]}"#,
        );
        let gh = GitHub::new(http).with_endpoints(
            "https://api.test",
            "https://codeload.test",
            "https://raw.test",
        );
        let hit = SearchHit {
            id: "x".into(),
            skill_id: "pdf".into(),
            name: "PDF".into(),
            installs: 0,
            source: "o/pdf".into(),
        };
        let found = resolve(&gh, &hit, Some("main")).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, "");
        assert_eq!(found[0].dir_name(), "pdf");
        // The default branch was not asked for, because a ref was given.
        assert!(
            !gh.http()
                .urls()
                .contains(&"https://api.test/repos/o/pdf".to_string())
        );
    }

    /// A repository holding one `pdf`, wired for both lookups.
    fn described_repo() -> FakeHttp {
        let http = FakeHttp::new();
        http.json(
            "https://api.test/repos/o/r",
            r#"{"default_branch":"trunk"}"#,
        );
        http.json(
            "https://api.test/repos/o/r/git/ref/heads/trunk",
            r#"{"object":{"sha":"c1","type":"commit"}}"#,
        );
        http.json(
            "https://api.test/repos/o/r/git/trees/c1?recursive=1",
            r#"{"sha":"root","truncated":false,"tree":[
                {"path":"skills/pdf/SKILL.md","type":"blob","sha":"b1"}]}"#,
        );
        http
    }

    fn described_hit() -> SearchHit {
        SearchHit {
            id: "cm7".into(),
            skill_id: "pdf".into(),
            name: "pdf".into(),
            installs: 12,
            source: "o/r".into(),
        }
    }

    #[test]
    fn a_hit_is_described_by_its_own_skill_file() {
        let http = described_repo();
        http.reply(
            "https://raw.test/o/r/c1/skills/pdf/SKILL.md",
            HttpResponse::new(
                200,
                b"---\nname: pdf\ndescription:  Fill in PDF forms.  \n---\n\nBody.\n".to_vec(),
            ),
        );
        let gh = GitHub::new(http).with_endpoints(
            "https://api.test",
            "https://codeload.test",
            "https://raw.test",
        );

        let detail = describe(&gh, &described_hit()).unwrap().unwrap();
        assert_eq!(detail.path, "skills/pdf");
        assert_eq!(detail.reference, "trunk");
        assert_eq!(detail.description.as_deref(), Some("Fill in PDF forms."));
    }

    #[test]
    fn a_skill_file_with_nothing_to_say_still_names_where_it_is() {
        // No frontmatter at all, which is not a reason to withhold the path:
        // the path is what tells two rows of the same name apart.
        let http = described_repo();
        http.reply(
            "https://raw.test/o/r/c1/skills/pdf/SKILL.md",
            HttpResponse::new(200, b"Just a heading.\n".to_vec()),
        );
        let gh = GitHub::new(http).with_endpoints(
            "https://api.test",
            "https://codeload.test",
            "https://raw.test",
        );

        let detail = describe(&gh, &described_hit()).unwrap().unwrap();
        assert_eq!(detail.path, "skills/pdf");
        assert_eq!(detail.description, None);
    }

    #[test]
    fn a_hit_the_repository_does_not_hold_describes_nothing() {
        let http = FakeHttp::new();
        http.json(
            "https://api.test/repos/o/r",
            r#"{"default_branch":"trunk"}"#,
        );
        http.json(
            "https://api.test/repos/o/r/git/ref/heads/trunk",
            r#"{"object":{"sha":"c1","type":"commit"}}"#,
        );
        http.json(
            "https://api.test/repos/o/r/git/trees/c1?recursive=1",
            r#"{"sha":"root","truncated":false,"tree":[
                {"path":"skills/docx/SKILL.md","type":"blob","sha":"b1"}]}"#,
        );
        let gh = GitHub::new(http).with_endpoints(
            "https://api.test",
            "https://codeload.test",
            "https://raw.test",
        );
        assert_eq!(describe(&gh, &described_hit()).unwrap(), None);
    }

    #[test]
    fn a_hit_whose_source_is_not_a_repository_is_refused() {
        let http = FakeHttp::new();
        let gh = GitHub::new(http).with_endpoints(
            "https://api.test",
            "https://codeload.test",
            "https://raw.test",
        );
        let hit = SearchHit {
            source: "nonsense".into(),
            ..SearchHit {
                id: String::new(),
                skill_id: "pdf".into(),
                name: String::new(),
                installs: 0,
                source: String::new(),
            }
        };
        assert!(resolve(&gh, &hit, None).is_err());
        assert_eq!(gh.http().request_count(), 0);
    }
}
