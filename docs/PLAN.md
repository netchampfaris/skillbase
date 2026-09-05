# Skillbase — Plan

A native desktop app (macOS + Linux) to manage Agent Skills across every coding
agent installed on the machine.

## The problem

Every coding agent keeps skills somewhere else. A skill that Claude Code can see
is invisible to Codex, Cursor, or opencode, and the only way to change that today
is to copy directories by hand. The user should think about the skill, not about
where each agent happens to look for it.

## The shape of the solution

One canonical store owned by Skillbase. Every agent gets a link into it.
Visibility is a property the user sets, not a directory they manage.

```
~/.skillbase/skills/<skill-name>/SKILL.md     canonical source of truth
        │
        ├── symlink ──> ~/.claude/skills/<skill-name>
        ├── symlink ──> ~/.codex/skills/<skill-name>
        └── (absent)     ~/.cursor/...              not visible to Cursor
```

"Make this skill visible only to Claude Code and Codex" becomes: keep those two
links, remove the rest. Nothing is copied, so an edit lands everywhere at once.

## Build order

1. **Foundation** — gpui-kit 0.6 app that opens a window. Confirms the toolchain,
   the dependency tree, and the render loop before any product code exists.
2. **Domain core** — `Skill`, `SkillSource`, `AgentTarget`; parse and write
   `SKILL.md` (YAML frontmatter + Markdown body) without losing unknown fields.
3. **Discovery** — scan every known agent root, plus project-local `.claude/skills`
   style directories under the user's code folders. Produce one inventory with,
   for each skill, the set of agents that can currently see it.
4. **Adoption + linking** — move a discovered skill into the canonical store and
   replace the original with a link; create and remove links per agent; fall back
   to copying where an agent does not follow symlinks.
5. **UI shell** — three panes in the Codex desktop idiom: sources and filters on
   the left, the skill list in the middle, detail on the right.
6. **Editor** — gpui-kit `Editor` with Markdown and YAML highlighting for the body,
   plus structured fields for `name` and `description`.
7. **Visibility controls** — per-agent toggles on the detail pane, with the
   filesystem effect of each toggle stated plainly in the UI.
8. **Create / install / remove** — new skill from template, install from a folder
   or archive, delete with confirmation.
9. **Polish** — custom titlebar, theme matched to Codex desktop, empty states,
   notifications, keyboard shortcuts.

## Constraints

- One dependency: `gpui-kit`. GPUI is never listed directly.
- No raw hex in application code. Colors come from `cx.theme()` semantic roles;
  the Codex-like palette is defined once in a theme layer.
- Never write into an agent's directory without the user asking. Discovery is
  read-only; every mutation is an explicit action with a visible consequence.
- Cross-platform paths from day one. macOS is what we can test here; Linux paths
  are resolved through the same abstraction rather than hardcoded.

## Verification

The app is driven on screen with computer use — launch it, screenshot it, click
through discovery, editing, and visibility toggles — not just compiled.
