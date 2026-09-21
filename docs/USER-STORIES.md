# User stories

Written from the user's point of view. Each story is a thing someone sits down
to do, not a feature.

## A. Getting started

A1. As a new user, I open Skillbase for the first time and see the skills I
    already have on disk, without having to import anything.
A2. As a new user with no skills at all, I see an empty state that tells me
    where to get one, not a blank pane.
A3. As a user, I understand which agents Skillbase found installed on my
    machine and which it did not.
A4. As a cautious user, I can tell that scanning changed nothing on disk.

## B. Installing

B1. As a user, I install one skill from a GitHub repository by pasting its URL.
B2. As a user, I search a registry (skills.sh) and install a skill from the
    result without knowing its repository URL.
B3. As a user, I install several skills in one go: I pick more than one search
    result, or paste a repo that holds many skills, and install them together.
B4. As a user, I install a skill from a subdirectory of a repo, and from a
    branch or tag that is not the default.
B5. As a user installing, I see progress, and I can cancel a download that is
    taking too long or that I started by mistake.
B6. As a user, when an install fails (network, 404, private repo, rate limit, a
    directory with no SKILL.md) I am told what failed and what to do about it.
B7. As a user, when the skill I am installing already exists, I am told, and I
    choose between replacing it, keeping both, or cancelling.
B8. As a user, after installing I land on the new skill, and I can immediately
    choose which agents see it.
B9. As a user, I install a skill from a local directory I already have.
B10. As a user with a private repo, I authenticate once and install from it.

## C. Removing

C1. As a user, I delete a skill and every symlink to it goes away with it.
C2. As a user, I am asked to confirm a delete, and told what will be removed
    and from where.
C3. As a user, I select several skills and delete them in one action.
C4. As a user, I can undo a delete I did not mean, or at least be told where
    the files went.
C5. As a user, I remove a skill from one agent without deleting the skill.

## D. Visibility per agent

D1. As a user, I turn one agent's access to a skill on or off with one switch.
D2. As a user, I give a skill to every agent, or take it from every agent, in
    one action rather than fourteen switches.
D3. As a user, I select several skills and enable them for one agent at once.
D4. As a user, when a switch cannot be honoured (agent not installed, a real
    file is in the way, no write permission) I am told why, not left with a
    switch that silently flips back.
D5. As a user, I see at a glance, from the list, which agents each skill
    reaches.

## E. Finding things

E1. As a user with a hundred skills, I search by name and by description.
E2. As a user, I filter the list to one agent, to a scope, or to skills that
    have an update.
E3. As a user, I sort by name or by how often I have used a skill, and I
    understand what a zero count means.
E4. As a user, I move through the list and open a skill with the keyboard.
E5. As a user, when a search returns nothing, I am told so and offered the next
    step (search the registry instead).

## F. Editing

F1. As a user, I edit SKILL.md and save it, and nothing else in the file
    changes.
F2. As a user, I edit the name and description as fields, without touching YAML.
F3. As a user, I am warned before I close or navigate away from unsaved edits.
F4. As a user, I open other files in the skill directory and save them.
F5. As a user, a skill with broken YAML still opens so I can repair it.
F6. As a user, I create a new skill from scratch inside Skillbase.
F7. As a user, I reveal the skill in Finder or open it in my editor.

## G. Updates

G1. As a user, I am told when an installed skill has a newer version upstream.
G2. As a user, I update one skill, and I update every out-of-date skill at once.
G3. As a user, when I have local edits and there is an upstream change, I am
    told what the conflict is and I choose.
G4. As a user, I check for updates when I want to, and I know when the last
    check happened.
G5. As a user, I see what changed before I accept an update.

## H. Duplicates

H1. As a user, I am told that copies of the same skill are scattered across
    agent directories.
H2. As a user, I consolidate the identical copies into symlinks in one action.
H3. As a user, I see which copies differ from the original, and what the
    difference is, before I overwrite them.

## I. Trust and recovery

I1. As a user, before anything is written to disk I know what will change.
I2. As a user, when an operation fails halfway, I am told the state it left
    behind.
I3. As a user, I can undo the last destructive action, or find the backup.
I4. As a user, I try Skillbase against a throwaway home without risking my real
    one, and the window makes that mode obvious.

## J. The window itself

J1. As a user, the app remembers my window size, selection, sort and filters
    between launches.
J2. As a user, every action I can take with the mouse has a menu item and a
    keyboard shortcut.
J3. As a user, long operations never freeze the window.
J4. As a user, the app follows my system appearance and accent colour.
