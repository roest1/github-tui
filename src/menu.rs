//! A row's actions, as a screen.
//!
//! The three-dots menu from the design doc, and the mechanism that makes "full
//! functionality, all in a TUI" affordable. It needs nothing from `tui-core`
//! that was not already there: a menu is a list of labelled rows, `Flow::Push`
//! opens it over the screen it came from, and `q` closes it.
//!
//! ## Why a menu rather than focusable buttons on the row
//!
//! The design draws `[PR]`, a trash can and a `…` on each branch row. The
//! FUNCTIONS all belong -- this repo's own rule is that a key you have to be
//! told about is a key nobody finds, so a visible, labelled action beats a
//! hidden keybinding. What a menu avoids is the only expensive part: putting
//! the cursor ON a button inside a row needs column-wise focus, a second
//! navigation axis, and it would collide with `h`/`l` meaning close/open
//! everywhere else. A menu gets the same functionality on one axis.
//!
//! The bash implementation already worked this way -- `enter` on a PR row
//! opens thirteen actions, `enter` on a branch row opens seven -- so this is
//! the shape being ported, not a new idea.
//!
//! ## Why most items are `Flow::Detach`
//!
//! Because `gh` and `git` are already interactive. `gh pr create` prompts for a
//! title and body; `gh pr edit` prompts for what to change; `git push --delete`
//! prints what it did and fails loudly. Handing them the real terminal gets
//! text entry, confirmation and error reporting for free, in the words the
//! tools themselves use -- so "full functionality" does NOT wait on a text
//! input widget in `tui-core`. That was the surprise of building this.
//!
//! ## How the screen underneath finds out
//!
//! It cannot ask: a pushed screen is gone by the time the one under it is
//! looked at again. So a screen that wants to know hands its menus a `Writes`
//! -- a shared count -- with `Menu::marks`, and the menu adds one each time
//! it hands a command to the terminal. Looking at a menu and closing it adds
//! nothing, so nothing is fetched again for it.

use std::cell::Cell;
use std::rc::Rc;

use tui_core::{nav::Action, Flow, Pane, Row, Screen};

use crate::paint::{BOLD, DIM, GRY, OFF, ORG, RED};

/// How many times something may have written: to GitHub, or to a clone.
///
/// Shared between a screen and whatever it pushes, and read by the screen
/// once it is on top again. A count, not a flag, because several screens can
/// read one -- the environment list, one environment's menu and the reviewer
/// picker do -- and each has its own copy to throw away: a flag the first of
/// them cleared would leave the others acting on theirs. Each remembers the
/// count it last re-read at.
#[derive(Clone, Default)]
pub struct Writes(Rc<Cell<u64>>);

impl Writes {
    pub(crate) fn mark(&self) {
        self.0.set(self.0.get() + 1);
    }

    pub(crate) fn count(&self) -> u64 {
        self.0.get()
    }

    /// Has anything been written since `seen`? Catches `seen` up, so the
    /// answer is yes once per write however often it is asked.
    pub(crate) fn since(&self, seen: &mut u64) -> bool {
        let now = self.count();
        if now == *seen {
            return false;
        }
        *seen = now;
        true
    }
}

/// What an item does when chosen.
pub enum Act {
    /// Hand over the terminal and run this, then come back. The program is
    /// first; nothing goes through a shell.
    Run(Vec<String>),
    /// Open another screen on top. Built with `Act::open`.
    Open(Opens),
    /// Ask for one line, then run `argv` with the answer appended.
    ///
    /// For the handful of actions `gh` cannot prompt for itself: it will ask
    /// for a PR's title and body, but `gh api -X POST .../rename` wants a
    /// `new_name` as an argument. The template holds everything up to it.
    Prompt {
        label: String,
        initial: String,
        /// The command, with `{}` standing for the answer.
        argv: Vec<String>,
    },
}

/// The screen an item opens, and how to build it again.
pub struct Opens {
    ready: Option<Box<dyn Screen>>,
    make: Box<dyn Fn() -> Box<dyn Screen>>,
}

impl Act {
    /// Open the screen `make` builds.
    ///
    /// Built once HERE, so whatever it asks GitHub for is already on its way
    /// when the row is chosen -- and built again each time after that, because
    /// the menu is still there when the screen it opened is closed. This used
    /// to hold the one screen and hand it over, leaving a row that closed the
    /// menu: `q` out of the rules and `enter` on the same row again did that
    /// instead of opening them.
    pub fn open(make: impl Fn() -> Box<dyn Screen> + 'static) -> Self {
        Act::Open(Opens {
            ready: Some(make()),
            make: Box::new(make),
        })
    }
}

pub struct Item {
    pub label: String,
    /// Dim text beside the label: what it is about to do, in the tool's own
    /// terms. `gh pr merge --squash` rather than "squashes the commits".
    pub detail: String,
    /// Said in the pane, for anything whose consequences are worth a sentence
    /// before you press enter.
    pub note: Vec<String>,
    /// True for anything that cannot be undone: red, a `!` badge, and the
    /// pane says so. Implies `asks`.
    pub destructive: bool,
    /// Require a `y` before this runs.
    ///
    /// This used to be absent, on the reasoning that the tool being run does
    /// the confirming and two confirms is one too many. That premise was
    /// false and the distinction it missed is the whole point: `git push
    /// --delete` REPORTS afterwards, it does not ASK beforehand, and `gh api
    /// -X POST .../rename` does neither. So deleting a remote branch was one
    /// `enter` from a menu row, which is not a price anyone agreed to.
    ///
    /// Reporting is not confirming. Where the tool genuinely does ask -- `gh
    /// pr create` stopping for a title, `gh auth refresh` printing a code and
    /// waiting -- this stays false, and there is still only one prompt.
    pub asks: bool,
    /// Run the command from inside this directory, where it would otherwise
    /// run wherever `github` happened to be started. See `Item::in_dir`.
    pub dir: Option<std::path::PathBuf>,
    pub act: Act,
}

impl Menu {
    /// The items, for the structural tests that assert something about EVERY
    /// command a menu offers rather than about one of them.
    #[cfg(test)]
    pub(crate) fn items(&self) -> &[Item] {
        &self.items
    }
}

impl Item {
    pub fn new(label: impl Into<String>, detail: impl Into<String>, act: Act) -> Self {
        Self {
            label: label.into(),
            detail: detail.into(),
            note: Vec::new(),
            destructive: false,
            asks: false,
            dir: None,
            act,
        }
    }

    pub fn note(mut self, lines: &[&str]) -> Self {
        self.note = lines.iter().map(|s| s.to_string()).collect();
        self
    }

    /// Ask before running. For anything that changes a shared thing --
    /// a default branch, a branch everyone has tracked.
    pub fn confirm(mut self) -> Self {
        self.asks = true;
        self
    }

    /// Cannot be undone. Implies `confirm`, because the one thing worse than
    /// an unconfirmed change is an unconfirmed irreversible one.
    pub fn danger(mut self) -> Self {
        self.destructive = true;
        self.asks = true;
        self
    }

    /// Run it from inside `dir`.
    ///
    /// For the commands that act on the git repository they are standing in
    /// and have no flag to be told otherwise -- `gh pr checkout` fetches into
    /// whichever clone that is. The directory is handed to the terminal with
    /// the command (`Flow::DetachIn`) and shown with it, rather than folded
    /// into a `sh -c 'cd …'` that the pane would then have to show instead of
    /// the command.
    pub fn in_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.dir = Some(dir.into());
        self
    }
}

/// An argv broken over lines that fit, at argument boundaries.
///
/// `argv.join(" ")` was fine while every command was `gh pr merge 86 --repo
/// x`. An environment's PUT carries the whole environment -- a wait timer,
/// four reviewers, a branch policy -- and came to three hundred characters,
/// which the pane truncated. A confirm you cannot read all of is not a
/// confirm, so this wraps instead.
///
/// A flag keeps the value after it, because `-F` on one line and
/// `reviewers[][id]=7` on the next reads as two things.
fn wrap_argv(argv: &[String], width: usize) -> Vec<String> {
    let width = width.max(12);
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut i = 0;
    while i < argv.len() {
        let mut piece = argv[i].clone();
        if piece.starts_with('-') && argv.get(i + 1).is_some_and(|n| !n.starts_with('-')) {
            piece.push(' ');
            piece.push_str(&argv[i + 1]);
            i += 1;
        }
        i += 1;
        if line.is_empty() {
            line = piece;
        } else if tui_core::width::width(&line) + 1 + tui_core::width::width(&piece) <= width {
            line.push(' ');
            line.push_str(&piece);
        } else {
            out.push(std::mem::take(&mut line));
            line = piece;
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

pub struct Menu {
    subject: String,
    kind: String,
    items: Vec<Item>,
    sel: usize,
    /// Counted up each time a command is handed to the terminal, for the
    /// screen underneath to read once this menu is gone. See `marks`.
    marks: Option<Writes>,
    /// The item whose question is outstanding. The screen has to remember
    /// what it asked -- `app` carries the text and nothing else.
    asking: Option<usize>,
    /// An item waiting on a `y`, with the command already resolved -- so the
    /// prompt can show exactly what is about to run, including whatever was
    /// typed into a field a moment ago.
    pending: Option<(usize, Vec<String>)>,
}

impl Menu {
    pub fn new(kind: impl Into<String>, subject: impl Into<String>, items: Vec<Item>) -> Self {
        Self {
            subject: subject.into(),
            kind: kind.into(),
            items,
            sel: 0,
            marks: None,
            asking: None,
            pending: None,
        }
    }

    /// Count every command this menu runs in `writes`.
    ///
    /// For the screen that pushes it: what it shows may be out of date once
    /// something here has run, and only then. Marking itself stale as it
    /// opened the menu meant a re-read for a menu that was looked at and
    /// closed.
    pub fn marks(mut self, writes: &Writes) -> Self {
        self.marks = Some(writes.clone());
        self
    }

    /// Either run it, or ask first.
    fn go(&mut self, i: usize, argv: Vec<String>) -> Flow {
        if self.items.get(i).is_some_and(|it| it.asks) {
            self.pending = Some((i, argv));
            return Flow::Dirty;
        }
        self.run(i, argv)
    }

    /// Hand the command to the terminal, and count it.
    ///
    /// Counted as it leaves, not when it comes back: nothing here learns how
    /// it went. A command that failed costs the screen underneath one
    /// re-read; one that worked and went uncounted would leave it showing
    /// what is no longer there.
    fn run(&mut self, i: usize, argv: Vec<String>) -> Flow {
        if let Some(writes) = &self.marks {
            writes.mark();
        }
        match self.items.get(i).and_then(|item| item.dir.clone()) {
            Some(dir) => Flow::DetachIn { dir, argv },
            None => Flow::Detach(argv),
        }
    }
}

impl Screen for Menu {
    fn title(&self) -> String {
        format!("{} \u{b7} {}", self.kind, self.subject)
    }

    fn focus(&mut self, sel: usize) {
        // Any focus call withdraws a pending confirm, not only one that moves.
        // `app` calls this only when the cursor actually changed, so the
        // unconditional version is equivalent and has no guard to reason
        // about -- and the guarded version's subtlety is exactly the sort of
        // thing that leaves a delete armed.
        self.pending = None;
        self.sel = sel;
    }

    fn rows(&mut self) -> Vec<Row> {
        self.items
            .iter()
            .map(|i| {
                let r = Row::leaf(&i.label).detail(i.detail.clone());
                if i.destructive {
                    r.badged("!", crate::paint::idx::RED)
                } else {
                    r
                }
            })
            .collect()
    }

    fn pane(&mut self, sel: usize, cols: u16, _rows: u16) -> Pane {
        let mut lines = Vec::new();
        // The item a `y` is waiting on, not the one under the cursor. They
        // differ when the arrows moved it while a field was open, and the
        // prompt then named one action above the command for another.
        let about = self.pending.as_ref().map_or(sel, |(i, _)| *i);
        let Some(item) = self.items.get(about) else {
            return Pane { image: None, lines };
        };
        let wide = (cols as usize).saturating_sub(4);
        // Where it runs is part of what runs, so it is said with the command.
        let inside = item
            .dir
            .as_ref()
            .map(|dir| format!("  {GRY}in {}{OFF}", dir.display()));
        lines.push(format!("{BOLD}{}{OFF}", item.label));
        lines.push(format!("{GRY}{}{OFF}", self.subject));
        lines.push(crate::paint::rule(cols));
        if let Some((_, argv)) = &self.pending {
            lines.push(format!("{ORG}About to run:{OFF}"));
            lines.push(String::new());
            for line in wrap_argv(argv, wide) {
                lines.push(format!("  {BOLD}{line}{OFF}"));
            }
            lines.extend(inside.clone());
            lines.push(String::new());
            if item.destructive {
                lines.push(format!("{RED}This cannot be undone.{OFF}"));
                lines.push(String::new());
            }
            lines.push(format!(
                "{ORG}y{OFF} to go ahead \u{b7} any other key leaves it"
            ));
            return Pane { image: None, lines };
        }
        if item.destructive {
            lines.push(format!("{RED}This cannot be undone.{OFF}"));
            lines.push(String::new());
        }
        lines.extend(item.note.iter().cloned());
        if !item.note.is_empty() {
            lines.push(String::new());
        }
        match &item.act {
            // Showing the command is the honest version of a confirmation: you
            // can see exactly what is about to run, in the words you would
            // have typed.
            Act::Run(argv) => {
                lines.push(format!("{GRY}runs:{OFF}"));
                for line in wrap_argv(argv, wide) {
                    lines.push(format!("  {ORG}{line}{OFF}"));
                }
                lines.extend(inside);
                lines.push(String::new());
                lines.push(format!(
                    "{DIM}on the real terminal, so it can prompt and you see{OFF}"
                ));
                lines.push(format!("{DIM}its own output.{OFF}"));
            }
            Act::Open(_) => lines.push(format!("{DIM}opens a screen{OFF}")),
            Act::Prompt { argv, label, .. } => {
                lines.push(format!("{GRY}asks:{OFF}"));
                lines.push(format!("  {ORG}{label}{OFF}"));
                lines.push(String::new());
                lines.push(format!("{GRY}then runs:{OFF}"));
                for line in wrap_argv(argv, wide) {
                    lines.push(format!("  {ORG}{line}{OFF}"));
                }
            }
        }
        Pane { image: None, lines }
    }

    fn on_action(&mut self, action: Action, sel: usize) -> Flow {
        // A pending confirm consumes the very next key, whatever it is. Only
        // `y` acts; everything else withdraws. Safe by construction rather
        // than by bookkeeping -- a stale prompt cannot turn a later keystroke
        // into a delete, because the later keystroke is not a `y`.
        if let Some((i, argv)) = self.pending.take() {
            return match action {
                Action::Key('y') | Action::Key('Y') => self.run(i, argv),
                _ => Flow::Dirty,
            };
        }
        // Enter alone. `l` would be a second key that runs a delete, which is
        // the one place in this program where an extra way to act is a cost
        // rather than a convenience.
        if action != Action::Activate {
            return Flow::Continue;
        }
        let Some(item) = self.items.get_mut(sel) else {
            return Flow::Continue;
        };
        match &mut item.act {
            Act::Run(argv) => {
                let argv = argv.clone();
                self.go(sel, argv)
            }
            // The one built ahead of time, the first time; a new one after.
            Act::Open(opens) => Flow::Push(opens.ready.take().unwrap_or_else(|| (opens.make)())),
            Act::Prompt { label, initial, .. } => {
                let (label, initial) = (label.clone(), initial.clone());
                self.asking = Some(sel);
                Flow::Ask { label, initial }
            }
        }
    }

    /// The answer to the one question a menu can ask.
    fn answered(&mut self, text: Option<String>) -> Flow {
        let Some(i) = self.asking.take() else {
            return Flow::Continue;
        };
        // Abandoned, or typed nothing. An empty answer is a cancel rather than
        // a command run with an empty argument -- `--delete ""` is not
        // something to find out about afterwards.
        let Some(text) = text.filter(|t| !t.trim().is_empty()) else {
            return Flow::Dirty;
        };
        let Some(Act::Prompt { argv, .. }) = self.items.get(i).map(|it| &it.act) else {
            return Flow::Continue;
        };
        let filled: Vec<String> = argv.iter().map(|a| a.replace("{}", text.trim())).collect();
        // The confirm comes AFTER the field, so what it shows is the command
        // that will actually run, with the name just typed in it. Asking first
        // would be confirming a template.
        self.go(i, filled)
    }

    fn footer(&self) -> String {
        if self.pending.is_some() {
            return "y to go ahead \u{b7} any other key leaves it".into();
        }
        match self.items.get(self.sel) {
            Some(i) if i.destructive => "enter asks, then y \u{b7} cannot be undone".into(),
            Some(i) if i.asks => "enter asks, then y".into(),
            Some(_) => "enter runs it".into(),
            None => "nothing here".into(),
        }
    }
}

/// What the structural tests ask of every command a menu offers.
///
/// Each screen's test walks its own menus and counts its own items. What a
/// command has to satisfy is said once, here, so that one screen's test
/// cannot be quietly looser than another's.
#[cfg(test)]
pub(crate) mod check {
    use super::{Act, Item};

    /// The command an item runs, or the template of the one it asks a name
    /// for. `None` for an item that runs nothing.
    pub(crate) fn argv_of(item: &Item) -> Option<&Vec<String>> {
        match &item.act {
            Act::Run(argv) | Act::Prompt { argv, .. } => Some(argv),
            // Another screen, which answers for its own commands.
            Act::Open(_) => None,
        }
    }

    /// Does this command say which repository it means, rather than leaving
    /// it to the working directory?
    ///
    /// `github` is launched from anywhere -- that is what the repo index is
    /// for -- so a `gh` command has to name the repo and a `git` command has
    /// to carry `-C`. The slug has to be the VALUE of `--repo`: found merely
    /// somewhere in the arguments it could be a branch name or a field. `gh
    /// api` has no such flag and carries it in the path, and `gh repo` takes
    /// it as the argument after the verb.
    pub(crate) fn names_repo(argv: &[String], slug: &str) -> bool {
        let words: Vec<&str> = argv.iter().map(String::as_str).collect();
        match words.as_slice() {
            ["git", "-C", dir, ..] => !dir.is_empty(),
            ["git", ..] => false,
            ["gh", "api", rest @ ..] => {
                let path = format!("repos/{slug}/");
                rest.iter().any(|a| a.starts_with(&path))
            }
            ["gh", "repo", _, named, ..] => *named == slug,
            _ => words.windows(2).any(|w| w == ["--repo", slug]),
        }
    }

    /// Where a command stands with the rule that a change asks first.
    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum Kind {
        /// It changes nothing.
        Reads,
        /// It changes something, and gh stops and asks for itself before it
        /// does: a title, a comment's body, a secret's value. A second prompt
        /// in front of that is one too many.
        GhAsks,
        /// Everything else, which is the safe way round for a rule about
        /// deletes: the lists are of what need NOT ask, so a command added
        /// later has to ask unless someone says here that it need not.
        Writes,
    }

    pub(crate) fn kind(argv: &[String]) -> Kind {
        let words: Vec<&str> = argv.iter().map(String::as_str).collect();
        match words.as_slice() {
            ["gh", "pr", "view" | "diff", ..] | ["gh", "browse", ..] => Kind::Reads,
            ["gh", "pr", "create" | "comment", ..] | ["gh", "secret", "set", ..] => Kind::GhAsks,
            // With no flag saying what to change, gh asks what to edit.
            ["gh", "pr", "edit", rest @ ..] if !rest.iter().any(|a| a.starts_with("--add-")) => {
                Kind::GhAsks
            }
            _ => Kind::Writes,
        }
    }

    /// May this command run as it stands: it asks here, or it has no need to?
    pub(crate) fn asks_first(asks: bool, argv: &[String]) -> bool {
        asks || kind(argv) != Kind::Writes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared audit refuses what it exists to refuse. A slug that is only
    /// somewhere in the arguments does not name the repository, and a command
    /// nobody has listed as a read has to ask.
    #[test]
    fn the_audit_wants_the_repo_named_and_a_write_asked_about() {
        let v = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        let slug = "roest1/dotfiles";
        for named in [
            v(&["gh", "pr", "merge", "7", "--repo", slug]),
            v(&[
                "gh",
                "api",
                "-X",
                "DELETE",
                "repos/roest1/dotfiles/environments/x",
            ]),
            v(&["gh", "repo", "edit", slug, "--default-branch", "main"]),
            v(&["git", "-C", "/clone", "switch", "main"]),
            v(&["run", "cancel", "1", "--repo", slug]),
        ] {
            assert!(check::names_repo(&named, slug), "{named:?}");
        }
        for unnamed in [
            v(&["gh", "pr", "create", "--head", slug]),
            v(&["gh", "pr", "merge", "7", "--repo", "someone/else"]),
            v(&["gh", "pr", "merge", "7", "--repo"]),
            v(&[
                "gh",
                "api",
                "-X",
                "POST",
                "user/repos",
                "-f",
                "name=roest1/dotfiles",
            ]),
            v(&["gh", "repo", "edit", "--default-branch", slug]),
            v(&["git", "switch", "main", "-C"]),
        ] {
            assert!(!check::names_repo(&unnamed, slug), "{unnamed:?}");
        }

        assert!(check::asks_first(false, &v(&["gh", "pr", "view", "7"])));
        assert!(check::asks_first(false, &v(&["gh", "pr", "edit", "7"])));
        assert!(check::asks_first(false, &v(&["gh", "secret", "set", "X"])));
        for writes in [
            v(&["gh", "pr", "merge", "7"]),
            v(&["gh", "pr", "edit", "7", "--add-label", "x"]),
            v(&["gh", "secret", "delete", "X"]),
            v(&["git", "-C", "/clone", "push", "origin", "--delete", "x"]),
        ] {
            assert!(!check::asks_first(false, &writes), "{writes:?}");
            assert!(check::asks_first(true, &writes), "{writes:?}");
        }
    }

    /// `m`, and the count of what it runs.
    fn counted(m: Menu) -> (Menu, Writes) {
        let ran = Writes::default();
        (m.marks(&ran), ran)
    }

    fn menu() -> Menu {
        Menu::new(
            "branch",
            "feat/x",
            vec![
                Item::new(
                    "Checkout",
                    "git switch",
                    Act::Run(vec!["git".into(), "switch".into()]),
                ),
                Item::new("Delete", "git push --delete", Act::Run(vec!["git".into()])).danger(),
            ],
        )
    }

    /// A row that opens a screen can be chosen again once that screen is
    /// closed. It used to be good for one go and then close the menu.
    #[test]
    fn a_row_that_opens_a_screen_opens_it_every_time() {
        let mut m = Menu::new(
            "branch",
            "feat/x",
            vec![Item::new(
                "Effective rules",
                "",
                Act::open(|| Box::new(Menu::new("rules", "feat/x", Vec::new()))),
            )],
        );
        for go in 1..=3 {
            assert!(
                matches!(m.on_action(Action::Activate, 0), Flow::Push(_)),
                "enter number {go} did not open the screen"
            );
        }
    }

    /// The confirm names the item it is about to run, wherever the cursor is.
    #[test]
    fn the_confirm_is_about_the_pending_item_not_the_cursor_row() {
        let mut m = menu();
        m.on_action(Action::Activate, 1);
        let text = m.pane(0, 60, 40).lines.join("\n");
        assert!(text.contains("Delete"), "{text}");
        assert!(text.contains("cannot be undone"), "{text}");
        // And a pane too narrow to hold anything must not underflow.
        m.pane(1, 2, 40);
    }

    /// The pane SHOWS the command. That is the confirmation -- you read what is
    /// about to run in the words you would have typed, rather than answering a
    /// yes/no about a description of it.
    #[test]
    fn the_pane_names_the_command_before_you_run_it() {
        let mut m = menu();
        let text = m.pane(0, 60, 40).lines.join("\n");
        assert!(text.contains("git switch"), "{text}");
    }

    /// A long command wraps at argument boundaries rather than being cut, and
    /// a flag keeps the value that belongs to it. An environment PUT is three
    /// hundred characters, and a confirm you cannot read all of is not one.
    #[test]
    fn a_long_command_wraps_instead_of_being_truncated() {
        let argv: Vec<String> = [
            "gh",
            "api",
            "-X",
            "PUT",
            "repos/vercel/next.js/environments/bot-approved",
            "--silent",
            "-F",
            "wait_timer=5",
            "-F",
            "reviewers[][type]=Team",
            "-F",
            "reviewers[][id]=7",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let lines = wrap_argv(&argv, 44);
        assert!(lines.len() > 1, "{lines:?}");
        for l in &lines {
            assert!(tui_core::width::width(l) <= 44 || !l.contains(' '), "{l:?}");
        }
        // Nothing lost, and the flags kept their values.
        assert_eq!(lines.join(" "), argv.join(" "));
        assert!(
            lines.iter().any(|l| l.contains("-F wait_timer=5")),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.trim_end().ends_with("-F")),
            "a flag was orphaned: {lines:?}"
        );
    }

    /// A destructive item is marked in the row, in the pane and in the footer.
    /// One of the three is easy to miss.
    #[test]
    fn a_destructive_item_says_so_three_times() {
        let mut m = menu();
        let rows = m.rows();
        assert!(rows[0].badges.is_empty());
        assert_eq!(rows[1].badges.first().map(|b| b.0.as_str()), Some("!"));
        m.focus(1);
        assert!(m.footer().contains("cannot be undone"), "{}", m.footer());
        assert!(m
            .pane(1, 60, 40)
            .lines
            .join("\n")
            .contains("cannot be undone"));
    }

    /// The hole Riley spotted, and it was wider than rename: deleting a remote
    /// branch was ONE `enter` from a menu row. The note claimed the tool does
    /// the confirming, but `git push --delete` only REPORTS afterwards and
    /// `gh api -X POST` does neither. Reporting is not confirming.
    #[test]
    fn a_destructive_item_cannot_run_on_one_keypress() {
        let (mut m, ran) = counted(menu());
        // Item 1 is the delete. Enter asks rather than acting.
        assert!(matches!(m.on_action(Action::Activate, 1), Flow::Dirty));
        assert_eq!(ran.count(), 0, "it ran on the first enter");
        assert!(m.footer().contains("y to go ahead"));
        // The pane shows the command about to run, which IS the confirmation.
        assert!(m.pane(1, 60, 40).lines.join("\n").contains("About to run"));
        assert!(matches!(m.on_action(Action::Key('y'), 1), Flow::Detach(_)));
        assert_eq!(ran.count(), 1);
    }

    /// Anything that is not `y` withdraws -- including a second `enter`, which
    /// is the key most likely to be leaned on.
    #[test]
    fn only_y_goes_ahead() {
        for a in [
            Action::Activate,
            Action::Key('n'),
            Action::Key('d'),
            Action::Down,
            Action::Close,
        ] {
            let (mut m, ran) = counted(menu());
            m.on_action(Action::Activate, 1);
            assert!(
                !matches!(m.on_action(a, 1), Flow::Detach(_)),
                "{a:?} ran it"
            );
            assert_eq!(ran.count(), 0, "{a:?} ran it");
        }
        // And moving the cursor withdraws it.
        let mut m = menu();
        m.focus(1);
        m.on_action(Action::Activate, 1);
        assert!(m.pending.is_some());
        m.focus(0);
        assert!(m.pending.is_none());
    }

    /// A prompt confirms AFTER the field, so what gets approved is the command
    /// with the name just typed in it rather than a template.
    #[test]
    fn a_prompt_that_confirms_shows_the_filled_command() {
        let (mut m, ran) = counted(Menu::new(
            "branch",
            "feat/x",
            vec![Item::new(
                "Rename",
                "gh api",
                Act::Prompt {
                    label: "to".into(),
                    initial: "feat/x".into(),
                    argv: vec!["gh".into(), "-f".into(), "new_name={}".into()],
                },
            )
            .confirm()],
        ));
        assert!(matches!(m.on_action(Action::Activate, 0), Flow::Ask { .. }));
        // The answer does not run it -- it asks.
        assert!(matches!(m.answered(Some("feat/y".into())), Flow::Dirty));
        assert_eq!(ran.count(), 0);
        let pane = m.pane(0, 60, 40).lines.join("\n");
        assert!(pane.contains("new_name=feat/y"), "{pane}");
        match m.on_action(Action::Key('y'), 0) {
            Flow::Detach(argv) => assert_eq!(argv.last().unwrap(), "new_name=feat/y"),
            _ => panic!("y must run it"),
        }
    }

    /// Choosing an item detaches and counts it, which is how the screen
    /// underneath knows to refetch once this menu is gone.
    #[test]
    fn running_an_item_is_counted_for_the_screen_underneath() {
        let (mut m, ran) = counted(menu());
        assert_eq!(ran.count(), 0);
        assert!(matches!(m.on_action(Action::Activate, 0), Flow::Detach(_)));
        assert_eq!(ran.count(), 1);
    }

    /// A prompt asks before it runs, and runs with the answer substituted.
    #[test]
    fn a_prompt_fills_the_template_with_what_was_typed() {
        let (mut m, ran) = counted(Menu::new(
            "branch",
            "feat/x",
            vec![Item::new(
                "Rename",
                "gh api",
                Act::Prompt {
                    label: "rename feat/x to".into(),
                    initial: "feat/x".into(),
                    argv: vec!["gh".into(), "api".into(), "-f".into(), "new_name={}".into()],
                },
            )],
        ));
        // Enter asks rather than acting.
        match m.on_action(Action::Activate, 0) {
            Flow::Ask { label, initial } => {
                assert_eq!(label, "rename feat/x to");
                assert_eq!(initial, "feat/x", "a rename starts from the old name");
            }
            _ => panic!("a prompt must ask first"),
        }
        assert_eq!(ran.count(), 0, "nothing has run yet");
        match m.answered(Some("  feat/y  ".into())) {
            // Trimmed: a trailing space from a paste must not become part of
            // the branch name.
            Flow::Detach(argv) => assert_eq!(argv.last().unwrap(), "new_name=feat/y"),
            _ => panic!("an answer must run the command"),
        }
        assert_eq!(ran.count(), 1);
    }

    /// An abandoned or empty answer runs NOTHING. `--delete ""` is not
    /// something to find out about afterwards.
    #[test]
    fn an_empty_answer_is_a_cancel_rather_than_an_empty_argument() {
        let make = || {
            Menu::new(
                "branch",
                "feat/x",
                vec![Item::new(
                    "Rename",
                    "gh api",
                    Act::Prompt {
                        label: "to".into(),
                        initial: String::new(),
                        argv: vec!["gh".into(), "-f".into(), "new_name={}".into()],
                    },
                )],
            )
        };
        for answer in [None, Some(String::new()), Some("   ".into())] {
            let (mut m, ran) = counted(make());
            m.on_action(Action::Activate, 0);
            assert!(
                !matches!(m.answered(answer.clone()), Flow::Detach(_)),
                "{answer:?} ran a command"
            );
            assert_eq!(ran.count(), 0, "{answer:?} marked the menu as having run");
        }
    }

    /// An answer arriving when nothing was asked must not run the row the
    /// cursor happens to be on.
    #[test]
    fn an_unexpected_answer_does_nothing() {
        let (mut m, ran) = counted(menu());
        assert!(matches!(m.answered(Some("yes".into())), Flow::Continue));
        assert_eq!(ran.count(), 0);
    }

    /// Only enter acts. Every other key has to fall through, or a stray
    /// keystroke in a menu holding a delete would run it.
    #[test]
    fn no_other_key_can_trigger_an_item() {
        let (mut m, ran) = counted(menu());
        for a in [
            Action::Down,
            Action::Key('d'),
            Action::Key('y'),
            Action::Close,
        ] {
            assert!(matches!(m.on_action(a, 1), Flow::Continue), "{a:?} acted");
        }
        assert_eq!(ran.count(), 0);
    }
}
