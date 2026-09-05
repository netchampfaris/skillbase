//! Placeholder data for the application shell.
//!
//! Nothing here touches the filesystem. `skillbase-core` grows the real
//! discovery and registry layers separately; this module exists so the three
//! panes can be laid out, themed, and driven before that lands, and it is
//! meant to be deleted when the two are wired together.

use gpui_kit::SharedString;

/// A coding agent Skillbase can make a skill visible to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Agent {
    /// Stable identifier, used for element identity and for matching a skill's
    /// visibility list. Never a display string.
    pub id: &'static str,
    /// The name shown to the user.
    pub label: &'static str,
    /// The plain-words consequence of turning this agent's switch on.
    pub effect: &'static str,
}

/// The agents detected on this machine. Hard-coded until discovery lands.
pub const AGENTS: [Agent; 6] = [
    Agent {
        id: "claude-code",
        label: "Claude Code",
        effect: "Links into ~/.claude/skills",
    },
    Agent {
        id: "codex",
        label: "Codex",
        effect: "Links into ~/.codex/skills",
    },
    Agent {
        id: "cursor",
        label: "Cursor",
        effect: "Links into ~/.cursor/skills",
    },
    Agent {
        id: "gemini-cli",
        label: "Gemini CLI",
        effect: "Links into ~/.gemini/skills",
    },
    Agent {
        id: "opencode",
        label: "opencode",
        effect: "Links into ~/.config/opencode/skills",
    },
    Agent {
        id: "goose",
        label: "Goose",
        effect: "Links into ~/.config/goose/skills",
    },
];

/// Look up an agent's display label by id.
pub fn agent_label(id: &str) -> &'static str {
    AGENTS
        .iter()
        .find(|agent| agent.id == id)
        .map(|agent| agent.label)
        .unwrap_or("Unknown")
}

/// The rows of the sidebar's Library group.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Library {
    All,
    Shared,
    Managed,
    Unmanaged,
    Invalid,
}

impl Library {
    pub const ALL: [Library; 5] = [
        Library::All,
        Library::Shared,
        Library::Managed,
        Library::Unmanaged,
        Library::Invalid,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Library::All => "All",
            Library::Shared => "Shared",
            Library::Managed => "Managed",
            Library::Unmanaged => "Unmanaged",
            Library::Invalid => "Invalid",
        }
    }

    fn matches(self, skill: &Skill) -> bool {
        match self {
            Library::All => true,
            Library::Shared => skill.shared,
            Library::Managed => skill.managed,
            Library::Unmanaged => !skill.managed,
            Library::Invalid => !skill.valid,
        }
    }
}

/// What the skill list is currently showing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Library(Library),
    /// Every skill the agent with this id can currently see.
    Agent(&'static str),
}

impl Scope {
    /// The heading the skill list shows for this scope.
    pub fn title(self) -> &'static str {
        match self {
            Scope::Library(library) => library.label(),
            Scope::Agent(id) => agent_label(id),
        }
    }

    /// Whether this scope lists the given skill.
    pub fn shows(self, skill: &Skill) -> bool {
        match self {
            Scope::Library(library) => library.matches(skill),
            Scope::Agent(id) => skill.sees(id),
        }
    }
}

/// One skill, grouped from every copy of it on disk.
#[derive(Clone, Debug)]
pub struct Skill {
    pub name: SharedString,
    pub description: SharedString,
    /// Ids of the agents that can reach this skill through their own directory.
    pub agents: Vec<&'static str>,
    /// Whether a link exists in the vendor-neutral `~/.agents/skills`.
    pub shared: bool,
    /// Whether the origin sits inside the Skillbase store.
    pub managed: bool,
    /// Whether the frontmatter passes validation.
    pub valid: bool,
    /// The directory holding the actual bytes.
    pub origin: SharedString,
    /// The whole `SKILL.md`.
    pub body: SharedString,
}

impl Skill {
    /// Whether the agent with this id can currently reach the skill, directly
    /// or through the shared directory.
    pub fn sees(&self, agent: &str) -> bool {
        // Every agent in the placeholder set except Claude Code reads the
        // shared directory as a fallback.
        (self.shared && agent != "claude-code") || self.agents.contains(&agent)
    }

    /// The agent labels shown as badges under the skill's description, in the
    /// order the sidebar lists them so the row reads as a stable lane.
    pub fn visible_to(&self) -> Vec<&'static str> {
        AGENTS
            .iter()
            .filter(|agent| self.sees(agent.id))
            .map(|agent| agent.label)
            .collect()
    }

    fn matches_query(&self, query: &str) -> bool {
        if query.is_empty() {
            return true;
        }
        let query = query.to_lowercase();
        self.name.to_lowercase().contains(&query)
            || self.description.to_lowercase().contains(&query)
    }
}

/// The skills the shell displays until discovery is wired in.
pub fn placeholder_skills() -> Vec<Skill> {
    [
        (
            "code-review",
            "Review a diff for correctness bugs and reuse cleanups before the change is opened.",
            vec!["claude-code", "codex"],
            true,
            true,
            true,
            "~/.skillbase/store/code-review",
        ),
        (
            "pdf-processing",
            "Extract text and tables from PDF files. Use when the user mentions PDFs, forms, or scanned documents.",
            vec![],
            true,
            true,
            true,
            "~/.skillbase/store/pdf-processing",
        ),
        (
            "writing-style",
            "House style for prose: plain statements, no metaphor, no filler.",
            vec!["claude-code"],
            false,
            true,
            true,
            "~/.skillbase/store/writing-style",
        ),
        (
            "release-notes",
            "Turn a merged milestone into release notes grouped by what changed for the reader.",
            vec!["codex", "cursor"],
            false,
            false,
            true,
            "~/.codex/skills/release-notes",
        ),
        (
            "sql-explain",
            "Read a slow query plan and name the index or rewrite that would fix it.",
            vec!["gemini-cli"],
            true,
            true,
            true,
            "~/.skillbase/store/sql-explain",
        ),
        (
            "terraform-review",
            "Check a Terraform plan for destructive replacements and missing lifecycle rules.",
            vec!["opencode", "goose"],
            false,
            false,
            true,
            "~/.config/opencode/skills/terraform-review",
        ),
        (
            "api-docs",
            "Draft reference documentation from a handler signature and its tests.",
            vec![],
            true,
            false,
            true,
            "~/.agents/skills/api-docs",
        ),
        (
            "incident-report",
            "",
            vec!["claude-code"],
            false,
            false,
            false,
            "~/.claude/skills/incident-report",
        ),
    ]
    .into_iter()
    .map(
        |(name, description, agents, shared, managed, valid, origin)| Skill {
            name: name.into(),
            description: description.into(),
            agents,
            shared,
            managed,
            valid,
            origin: origin.into(),
            body: skill_body(name, description).into(),
        },
    )
    .collect()
}

/// Count the skills a scope would show.
pub fn count(skills: &[Skill], scope: Scope) -> usize {
    skills.iter().filter(|skill| scope.shows(skill)).count()
}

/// The skills a scope shows, narrowed by the search query.
pub fn filter<'a>(skills: &'a [Skill], scope: Scope, query: &str) -> Vec<&'a Skill> {
    skills
        .iter()
        .filter(|skill| scope.shows(skill) && skill.matches_query(query))
        .collect()
}

fn skill_body(name: &str, description: &str) -> String {
    let title = name
        .split('-')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");

    format!(
        "---\n\
         name: {name}\n\
         description: {description}\n\
         license: Apache-2.0\n\
         metadata:\n  \
         version: \"1.0\"\n\
         ---\n\
         \n\
         # {title}\n\
         \n\
         Instructions the agent reads when this skill fires.\n\
         \n\
         ## When to use\n\
         \n\
         - The user asks for {name} by name.\n\
         - The work in front of you matches what the description covers.\n\
         \n\
         ## Steps\n\
         \n\
         1. Read the files the task names before changing any of them.\n\
         2. Make the smallest change that answers the request.\n\
         3. Run the checks that would catch a regression.\n\
         \n\
         ```bash\n\
         cargo fmt && cargo clippy --all-targets -- -D warnings\n\
         ```\n"
    )
}
