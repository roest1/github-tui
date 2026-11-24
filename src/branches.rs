//! Branches: a full-width table, grouped Default / yours / all.
//!
//! The first screen to use `Split::Full`, and the reason that exists. A branch
//! row carries six facts -- protection, when, ahead/behind, who, the subject,
//! its checks and its PR -- and squeezing those into a 52-column sidebar beside
//! a pane with nothing to say would have meant dropping columns to make room
//! for emptiness.
//!
//! ## Nearly all of it comes from local git
//!
//! This is the shape the bash implementation found and it is worth preserving
//! exactly: `git for-each-ref` gives every remote branch's age, author, subject
//! AND its ahead/behind count against the default branch in ONE call, locally,
//! instantly, offline. The API is asked only for the things git cannot know --
//! which of them are `protected`, which have an open PR, and how their checks
//! went.
//!
//! So the table is on screen before any request is made, and the three API
//! columns fill in after. `%(ahead-behind:)` needs git >= 2.41; older gits get
//! the same table without those two numbers rather than an error.
//!
//! ## Checks come from one list of runs
//!
//! A per-branch check status would be one API call per branch. But a workflow
//! run already carries its `head_branch`, so the newest run per branch IS the
//! check status. One call for the whole column.
//!
//! It is this screen's own call, not the one the Actions tab makes. That one
//! asks for a shorter page, and this one is read as plain JSON rather than
//! through `model::Run`, for the one field that type does not keep:
//! `head_repository`. A run started by a fork's pull request carries the
//! FORK's branch name, and without the repository beside it a stranger's
//! `main` is indistinguishable from this one.
//!
//! ## The three groups, and why "all" repeats "yours"
//!
//! github.com's own branches page does this, and the design doc asks for it:
//! Default on its own, then yours, then everything. The repetition is the
//! point -- "yours" is a shortcut to the handful you are working on, and "all"
//! is the complete list you search. Both are collapsible, so the duplication
//! costs a keystroke to be rid of.

use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::Deserialize;
use tui_core::{nav::Action, Flow, Pane, Row, Screen, Split, Tick};

use crate::gh::{Fetcher, Got, Req, PAGE};
use crate::menu::{Act, Item, Menu, Writes};
use crate::model::Verdict;
use crate::paint::{idx, DIM, GRY, OFF, ORG, RED};
use crate::time;

#[derive(Debug, Clone, Deserialize)]
struct ApiBranch {
    name: String,
    #[serde(default)]
    protected: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct RepoInfo {
    default_branch: Option<String>,
}

/// Which repository a ref lives in. The id, because it is the one thing about
/// a repository that a rename, a transfer or a differently-cased remote URL
/// does not change.
#[derive(Debug, Clone, Deserialize)]
struct RepoRef {
    id: u64,
}

/// Is `head` a branch of the repository `base` is in?
///
/// A ref NAME says nothing about where it lives: a fork's pull request has a
/// head called `main` as often as not. Keyed on the name alone, that pull
/// request's number and its checks were drawn on this repository's `main`.
/// A head with no repository is a fork that has since been deleted, so it is
/// not here either.
fn same_repo(head: &Option<RepoRef>, base: &Option<RepoRef>) -> bool {
    matches!((head, base), (Some(h), Some(b)) if h.id == b.id)
}

#[derive(Debug, Clone, Deserialize)]
struct Pr {
    number: u64,
    #[serde(default)]
    draft: bool,
    head: PrSide,
    base: PrSide,
}

#[derive(Debug, Clone, Deserialize)]
struct PrSide {
    #[serde(rename = "ref")]
    name: String,
    repo: Option<RepoRef>,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiRun {
    head_branch: Option<String>,
    status: Option<String>,
    conclusion: Option<String>,
    head_repository: Option<RepoRef>,
    repository: Option<RepoRef>,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiRuns {
    #[serde(default)]
    workflow_runs: Vec<ApiRun>,
}

/// What local git knows about one remote branch.
#[derive(Debug, Clone, Default)]
struct Facts {
    when: i64,
    who: String,
    /// Matched against `user.email` to decide whose branch this is. The NAME
    /// cannot do it: measured on this repo, the same person authored commits
    /// as both `Riley Oest` and `roest1` -- GitHub's web UI and a local
    /// `user.name` disagreeing -- while the email was identical on every one.
    /// Matching on the name silently filed your own branches under someone
    /// else.
    email: String,
    subject: String,
    /// `None` on git < 2.41, which has no `%(ahead-behind:)`.
    ahead_behind: Option<(u32, u32)>,
}

/// One branch, everything joined.
#[derive(Debug, Clone)]
struct Branch {
    name: String,
    protected: bool,
    facts: Option<Facts>,
}

/// What `git` told us, once, at open.
struct Local {
    default: Option<String>,
    current: Option<String>,
    /// Every branch under `refs/heads` -- the ones you have a copy of.
    ///
    /// The table is otherwise a list of REMOTE branches, which cannot answer
    /// "do I have this one" -- and that is the question in front of "should I
    /// check it out". The fzf preview said `remote only -- enter to check
    /// out`; here it is a marker, because it belongs on every row at once
    /// rather than on the one the cursor happens to be resting on.
    locals: std::collections::HashSet<String>,
    /// `user.email`, then `user.name` as a fallback for a repo configured with
    /// no email.
    me_email: Option<String>,
    me_name: Option<String>,
    facts: HashMap<String, Facts>,
}

impl Local {
    fn is_mine(&self, f: Option<&Facts>) -> bool {
        let Some(f) = f else { return false };
        if let Some(e) = self.me_email.as_deref().filter(|e| !e.is_empty()) {
            return f.email == e;
        }
        self.me_name.as_deref().is_some_and(|n| f.who == n)
    }
}

fn git(dir: &std::path::Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The default branch's name, from what `origin/HEAD` points at.
///
/// Only the remote's prefix comes off. A branch name may itself hold slashes,
/// and taking the last segment turned `origin/release/v2` into `v2` -- a name
/// no branch has, so there was no Default group and the real default was
/// offered Delete and Rename like any other branch. Measured: git was then
/// asked for ahead/behind against `origin/v2`, failed the whole command, and
/// the table lost both columns to the fallback meant for an old git.
fn default_of(symref: &str) -> Option<String> {
    symref
        .strip_prefix("refs/remotes/origin/")
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

/// Read every local fact in five `git` calls.
///
/// Synchronous, at open, which is the one place this crate shells out before
/// the first frame. It is affordable because all four read files -- no network,
/// no object walk beyond the refs -- and because the alternative is a table
/// that cannot draw until the API answers. `index` makes the same trade in the
/// other direction, and the note there says why.
fn read_local(dir: &std::path::Path) -> Local {
    // `refs/remotes/origin/HEAD` is a local symref written by clone, so the
    // default branch is known offline. The API's `default_branch` is the
    // fallback, and it arrives later.
    let default =
        git(dir, &["symbolic-ref", "refs/remotes/origin/HEAD"]).and_then(|s| default_of(&s));
    let current = git(dir, &["symbolic-ref", "--short", "HEAD"]);
    let me_email = git(dir, &["config", "user.email"]);
    let me_name = git(dir, &["config", "user.name"]);
    // `lstrip=2` drops `refs/heads/`, so these are directly comparable with
    // the remote names below, which `lstrip=3` strips to the same shape.
    let locals: std::collections::HashSet<String> = git(
        dir,
        &[
            "for-each-ref",
            "--format",
            "%(refname:lstrip=2)",
            "refs/heads",
        ],
    )
    .unwrap_or_default()
    .lines()
    .filter(|l| !l.is_empty())
    .map(str::to_string)
    .collect();

    let base = default.clone().unwrap_or_else(|| "HEAD".into());
    let fmt = format!(
        "%(refname:lstrip=3)%09%(committerdate:unix)%09%(authorname)%09\
         %(authoremail:trim)%09%(subject)%09%(ahead-behind:refs/remotes/origin/{base})"
    );
    // The fallback drops only the two numbers. Asking for an unsupported atom
    // makes git fail the WHOLE command, so there is no partial answer to take.
    let raw = git(
        dir,
        &["for-each-ref", "--format", &fmt, "refs/remotes/origin"],
    )
    .or_else(|| {
        let plain = "%(refname:lstrip=3)%09%(committerdate:unix)%09%(authorname)%09\
                     %(authoremail:trim)%09%(subject)";
        git(
            dir,
            &["for-each-ref", "--format", plain, "refs/remotes/origin"],
        )
    });

    let mut facts = HashMap::new();
    for line in raw.unwrap_or_default().lines() {
        let mut f = line.split('\t');
        let (Some(name), Some(when), Some(who), Some(email), Some(subject)) =
            (f.next(), f.next(), f.next(), f.next(), f.next())
        else {
            continue;
        };
        // `origin/HEAD` is a symref, not a branch; it would otherwise appear as
        // a row duplicating the default.
        if name.is_empty() || name == "HEAD" {
            continue;
        }
        let ahead_behind = f.next().and_then(|ab| {
            let mut n = ab.split_whitespace();
            Some((n.next()?.parse().ok()?, n.next()?.parse().ok()?))
        });
        facts.insert(
            name.to_string(),
            Facts {
                when: when.parse().unwrap_or(0),
                who: who.to_string(),
                email: email.to_string(),
                subject: subject.to_string(),
                ahead_behind,
            },
        );
    }
    Local {
        default,
        current,
        locals,
        me_email,
        me_name,
        facts,
    }
}

/// Which group a row sits in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Group {
    Default,
    Mine,
    All,
}

enum At {
    Head(usize),
    Branch(usize, usize),
    Nothing,
}

/// Why something only GitHub knows is not on screen, when the reason is that
/// GitHub refused or failed. `None` is "answered", or "not answered yet".
///
/// Kept per question rather than as one error, because each leaves a different
/// column blank and a blank column otherwise reads as a fact: no lock is "not
/// protected", no number is "no pull request".
#[derive(Default)]
struct Unknown {
    branches: Option<String>,
    prs: Option<String>,
    checks: Option<String>,
    default: Option<String>,
}

pub struct Branches {
    repo: String,
    dir: std::path::PathBuf,
    fetch: Arc<Fetcher>,
    local: Local,
    /// GitHub's branch list as it last answered. `None` until it does, and
    /// again whenever it refuses or fails -- the table is then git's alone.
    api: Option<Vec<ApiBranch>>,
    /// Derived by `rebuild` from `local` and `api`, never added to.
    branches: Vec<Branch>,
    /// Indices into `branches`, per group, in display order.
    groups: Vec<(Group, &'static str, Vec<usize>)>,
    open: Vec<bool>,
    prs: HashMap<String, (u64, bool)>,
    /// The pull request list came back a full page long, so there may be more.
    prs_cut: bool,
    checks: HashMap<String, Verdict>,
    epoch_seen: u64,
    unknown: Unknown,
    status: String,
    /// Where the cursor is -- and, straight after this screen has moved the
    /// rows under it, where it belongs. `initial_sel` answers with it.
    sel: usize,
    /// `f` handed the terminal to a fetch, so the next tick re-asks.
    stale: bool,
    /// Counted up by a branch's menu each time it runs something, and the
    /// count this table was last read at. A menu that was only looked at
    /// leaves the two equal, and nothing is asked again for it.
    writes: Writes,
    wrote_seen: u64,
}

/// Each asks for a full page, spelt out because a `const` cannot be built
/// with `format!`. An answer `PAGE` long may have been cut.
const BRANCHES: &str = "branches?per_page=100";
const PULLS: &str = "pulls?state=open&per_page=100";
const RUNS: &str = "actions/runs?per_page=100";
const REPO: &str = "";

/// `a`, `a and b`, `a, b and c`.
fn listed(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => one.to_string(),
        [most @ .., last] => format!("{} and {last}", most.join(", ")),
    }
}

impl Branches {
    pub fn open(repo: &str, dir: &std::path::Path, fetch: Arc<Fetcher>) -> Self {
        let local = read_local(dir);
        let mut me = Self {
            repo: repo.to_string(),
            dir: dir.to_path_buf(),
            fetch,
            local,
            api: None,
            branches: Vec::new(),
            groups: Vec::new(),
            open: Vec::new(),
            prs: HashMap::new(),
            prs_cut: false,
            checks: HashMap::new(),
            epoch_seen: 0,
            unknown: Unknown::default(),
            status: String::new(),
            sel: 0,
            stale: false,
            writes: Writes::default(),
            wrote_seen: 0,
        };
        me.ask();
        // Drawn from local git immediately, so the table is on screen before
        // any of the four requests land.
        me.rebuild();
        me
    }

    fn req(&self, tail: &str) -> Req {
        Req::Json {
            path: format!("repos/{}/{tail}", self.repo)
                .trim_end_matches('/')
                .to_string(),
        }
    }

    fn ask(&self) {
        for tail in [BRANCHES, PULLS, RUNS, REPO] {
            self.fetch.request(self.req(tail));
        }
    }

    fn refresh(&self) {
        for tail in [BRANCHES, PULLS, RUNS] {
            self.fetch.refresh(self.req(tail));
        }
    }

    /// Read git again and ask GitHub again.
    fn reread(&mut self) {
        self.refresh();
        self.adopt(read_local(&self.dir));
    }

    /// Take a fresh reading of local git and redraw from it at once.
    ///
    /// The second half is the point. A reading that waited for GitHub's answer
    /// to be drawn left the table showing the old one in the meantime, and
    /// for good if the answer never came.
    fn adopt(&mut self, local: Local) {
        self.local = local;
        self.sync();
    }

    /// One answer, parsed. `Ok(None)` is still on its way; `Err` is GitHub
    /// refusing or failing, kept as what it is rather than read as "nothing".
    fn read<T: DeserializeOwned>(&self, tail: &str, what: &str) -> Result<Option<T>, String> {
        match self.fetch.get(&self.req(tail)) {
            Some(Got::Json(v)) => T::deserialize(v.as_ref())
                .map(Some)
                .map_err(|e| format!("unreadable {what}: {e}")),
            Some(Got::Denied(d)) => Err(format!("the token may not read it: {}", d.fix())),
            Some(Got::Failed(e)) => Err(e),
            _ => Ok(None),
        }
    }

    /// Take whatever GitHub has answered so far.
    ///
    /// A refusal or a failure EMPTIES what it was about and records why. Left
    /// in place, last time's locks and pull request numbers would sit beside a
    /// line saying they are unknown.
    fn sync(&mut self) {
        match self.read::<Vec<ApiBranch>>(BRANCHES, "branch list") {
            Ok(Some(api)) => {
                self.api = Some(api);
                self.unknown.branches = None;
            }
            Ok(None) => {}
            Err(e) => {
                self.api = None;
                self.unknown.branches = Some(e);
            }
        }
        match self.read::<Vec<Pr>>(PULLS, "pull request list") {
            Ok(Some(prs)) => {
                self.prs_cut = prs.len() >= PAGE;
                self.prs = prs
                    .into_iter()
                    .filter(|p| same_repo(&p.head.repo, &p.base.repo))
                    .map(|p| (p.head.name, (p.number, p.draft)))
                    .collect();
                self.unknown.prs = None;
            }
            Ok(None) => {}
            Err(e) => {
                self.prs.clear();
                self.prs_cut = false;
                self.unknown.prs = Some(e);
            }
        }
        match self.read::<ApiRuns>(RUNS, "run list") {
            Ok(Some(list)) => {
                // Newest first from the API, so the first run seen for a
                // branch is its latest -- `entry().or_insert` keeps that one.
                let mut seen: HashMap<String, Verdict> = HashMap::new();
                for r in &list.workflow_runs {
                    if !same_repo(&r.head_repository, &r.repository) {
                        continue;
                    }
                    if let Some(b) = &r.head_branch {
                        seen.entry(b.clone()).or_insert_with(|| {
                            Verdict::of(r.status.as_deref(), r.conclusion.as_deref())
                        });
                    }
                }
                self.checks = seen;
                self.unknown.checks = None;
            }
            Ok(None) => {}
            Err(e) => {
                self.checks.clear();
                self.unknown.checks = Some(e);
            }
        }
        match self.read::<RepoInfo>(REPO, "repository") {
            Ok(Some(info)) => {
                // Only as a fallback: the local symref is authoritative for
                // what THIS clone thinks, and it is what the ahead/behind
                // numbers were computed against.
                if self.local.default.is_none() {
                    self.local.default = info.default_branch;
                }
                self.unknown.default = None;
            }
            Ok(None) => {}
            // Worth saying only when git could not answer either: with no
            // default there is no Default group, and the branch that should
            // be in it is offered Delete like any other.
            Err(e) => self.unknown.default = self.local.default.is_none().then_some(e),
        }
        self.rebuild();
    }

    /// Re-derive the table and its groups from whatever is known NOW.
    ///
    /// From nothing, every time. This used to add to the list it already had
    /// and never take away, and each row kept the facts it was first given:
    /// a branch deleted or renamed from its own menu stayed in the table,
    /// still offering Delete, and after a fetch every row went on showing the
    /// subject, age and ahead/behind from before it.
    fn rebuild(&mut self) {
        let facts = &self.local.facts;
        let mut branches: Vec<Branch> = self
            .api
            .iter()
            .flatten()
            .map(|b| Branch {
                name: b.name.clone(),
                protected: b.protected,
                facts: facts.get(&b.name).cloned(),
            })
            .collect();
        // GitHub's list, when it is the WHOLE list, says which branches exist:
        // a remote-tracking ref it does not name is one git has yet to prune,
        // which is what the old name looks like after a rename. Until GitHub
        // answers, or when its answer was cut at a page, git's refs are the
        // only word on the rest -- and that is how the table draws before the
        // first response.
        let whole = self.api.as_ref().is_some_and(|a| a.len() < PAGE);
        if !whole {
            let named: std::collections::HashSet<&str> =
                self.api.iter().flatten().map(|b| b.name.as_str()).collect();
            for (name, f) in facts {
                if !named.contains(name.as_str()) {
                    branches.push(Branch {
                        name: name.clone(),
                        protected: false,
                        facts: Some(f.clone()),
                    });
                }
            }
        }
        let when = |b: &Branch| b.facts.as_ref().map(|f| f.when).unwrap_or(0);
        branches.sort_by(|a, b| {
            when(b)
                .cmp(&when(a))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then_with(|| a.name.cmp(&b.name))
        });
        self.branches = branches;

        let def = self.local.default.clone().unwrap_or_default();
        let mut d = Vec::new();
        let mut mine = Vec::new();
        let mut all = Vec::new();
        for (i, b) in self.branches.iter().enumerate() {
            if b.name == def {
                d.push(i);
                continue;
            }
            if self.local.is_mine(b.facts.as_ref()) {
                mine.push(i);
            }
            all.push(i);
        }
        let keep = |v: Vec<usize>| !v.is_empty();
        let mut groups: Vec<(Group, &'static str, Vec<usize>)> = Vec::new();
        if keep(d.clone()) {
            groups.push((Group::Default, "\u{2b50} default", d));
        }
        if keep(mine.clone()) {
            groups.push((Group::Mine, "\u{1f331} your branches", mine));
        }
        if keep(all.clone()) {
            groups.push((Group::All, "\u{1f33f} all branches", all));
        }
        // Open state follows the group COUNT, so a group appearing after the
        // API answers does not shift which ones were collapsed.
        if self.open.len() != groups.len() {
            self.open = groups
                .iter()
                .map(|(g, _, _)| *g != Group::All || groups.len() == 1)
                .collect();
        }
        self.groups = groups;
    }

    /// What the table cannot show, and why, a line each.
    ///
    /// Said in the footer because this screen has no pane to say it in, and a
    /// row put above the table would push every branch down a line under the
    /// cursor at the moment an answer landed. What is missing comes FIRST and
    /// the reason after it, so a narrow terminal cuts the reason.
    fn notices(&self) -> Vec<String> {
        let mut by_reason: Vec<(&str, Vec<&str>)> = Vec::new();
        for (what, why) in [
            ("protection", &self.unknown.branches),
            ("pull requests", &self.unknown.prs),
            ("checks", &self.unknown.checks),
            ("the default branch", &self.unknown.default),
        ] {
            let Some(why) = why.as_deref() else { continue };
            // One line when they all failed the same way, which offline they do.
            match by_reason.iter_mut().find(|(r, _)| *r == why) {
                Some((_, whats)) => whats.push(what),
                None => by_reason.push((why, vec![what])),
            }
        }
        let mut out: Vec<String> = by_reason
            .iter()
            .map(|(why, whats)| format!("{RED}{} unknown{GRY} \u{2014} {why}{OFF}", listed(whats)))
            .collect();
        if self.api.as_ref().is_some_and(|a| a.len() >= PAGE) {
            out.push(format!(
                "{ORG}only github's first {PAGE} branches{GRY} \u{2014} past those, \
                 protection is unknown and a branch git has not fetched is missing{OFF}"
            ));
        }
        if self.prs_cut {
            out.push(format!(
                "{ORG}only the first {PAGE} open pull requests{GRY} \u{2014} a branch \
                 past those may have one{OFF}"
            ));
        }
        out
    }

    /// The row a group's heading is on.
    fn head_row(&self, g: usize) -> usize {
        self.groups
            .iter()
            .zip(&self.open)
            .take(g)
            // Its heading and its spacer, and its branches while it is open.
            .map(|((_, _, ids), open)| 2 + if *open { ids.len() } else { 0 })
            .sum()
    }

    fn at(&self, sel: usize) -> At {
        let mut row = 0;
        for (g, (_, _, ids)) in self.groups.iter().enumerate() {
            if sel == row {
                return At::Head(g);
            }
            row += 1;
            if self.open[g] {
                for (k, _) in ids.iter().enumerate() {
                    if sel == row {
                        return At::Branch(g, k);
                    }
                    row += 1;
                }
            }
            row += 1; // spacer
        }
        At::Nothing
    }

    fn branch_at(&self, sel: usize) -> Option<&Branch> {
        match self.at(sel) {
            At::Branch(g, k) => self.branches.get(self.groups[g].2[k]),
            _ => None,
        }
    }

    fn is_default(&self, name: &str) -> bool {
        self.local.default.as_deref() == Some(name)
    }

    fn is_current(&self, name: &str) -> bool {
        self.local.current.as_deref() == Some(name)
    }

    /// The row for one branch: the marker column, then the facts right-aligned
    /// as a table.
    fn row_for(&self, b: &Branch) -> Row {
        // Two fixed cells before the name, so every name starts in the same
        // column whether or not it is marked.
        //
        // The lock is back, which is the design doc's detail an earlier version
        // of this could not meet. `U+1F512` is East Asian Wide -- two cells
        // where `\u{2605}` is one -- and padding by character count put them in
        // different columns. `width::pad` measures CELLS, so the flag is
        // exactly two wide whatever is in it.
        // Three states in one cell, which is what keeps the column fixed:
        // you are ON it, you HAVE it, or it is only on the remote. The last
        // is the interesting one -- it is the branch `Checkout` would have to
        // create -- and it is the blank, because most of a long list is
        // branches you have never fetched.
        let here = if self.is_current(&b.name) {
            "\u{25cf}"
        } else if self.local.locals.contains(&b.name) {
            "\u{25cb}"
        } else {
            " "
        };
        let flag = if self.is_default(&b.name) {
            "\u{2b50}"
        } else if b.protected {
            "\u{1f512}"
        } else {
            ""
        };
        let flag = tui_core::width::pad(flag, 2);
        let mut row = Row::leaf(format!("{here}{flag} {}", b.name));
        // Depth 1, because a branch IS a child of its group -- and `depth` is
        // what says so. It was 0, which looked right because the marker column
        // supplies the visual indent anyway, and was wrong in two ways that
        // only showed up later: filtering could not tell which group owned a
        // surviving row, so a match appeared with its heading deleted; and `h`
        // on a branch had no parent to collapse.
        row.depth = 1;

        let f = b.facts.as_ref();
        row = row.detail(
            f.map(|f| f.subject.replace(['\t', '\n'], " "))
                .unwrap_or_else(|| "not fetched yet".into()),
        );

        // Badges in reading order, each padded to a FIXED width so they line up
        // as columns -- `app` packs them right but does not align them.
        let who = f.map(|f| f.who.as_str()).unwrap_or("");
        row = row.badged(format!("{:<14}", trunc(who, 14)), idx::GRY);

        let age = f
            .map(|f| time::age((time::now() - f.when).max(0)))
            .unwrap_or_default();
        row = row.badged(format!("{age:>4}"), idx::DIM);

        // The default branch is what the others are measured against, so it has
        // no ahead/behind of its own -- blanks rather than two zeros, which
        // would read as a fact.
        let ab = if self.is_default(&b.name) {
            " ".repeat(9)
        } else {
            match f.and_then(|f| f.ahead_behind) {
                Some((a, d)) => format!("\u{2191}{a:>3} \u{2193}{d:>3}"),
                None => " ".repeat(9),
            }
        };
        let ab_colour = match f.and_then(|f| f.ahead_behind) {
            Some((_, d)) if d > 0 => idx::YEL,
            Some(_) => idx::GRN,
            None => idx::DIM,
        };
        row = row.badged(ab, ab_colour);

        // Padded in CELLS. `{:>9}` counts characters, so every checked row
        // would come out one column narrower than the blank ones -- the exact
        // bug `tui_core::width` was written for.
        let (check, check_colour) = match self.checks.get(&b.name) {
            Some(v) => {
                let (t, c) = v.badge();
                (tui_core::width::rpad(t, 9), c)
            }
            None => (" ".repeat(9), idx::DIM),
        };
        row = row.badged(check, check_colour);

        let (pr, pr_colour) = match self.prs.get(&b.name) {
            Some((n, draft)) => (
                format!("{:>6}", format!("#{n}")),
                if *draft { idx::GRY } else { idx::GRN },
            ),
            None => (" ".repeat(6), idx::DIM),
        };
        row.badged(pr, pr_colour)
    }

    /// The branch's action menu -- the three-dots from the design.
    fn menu_for(&self, b: &Branch) -> Menu {
        let name = b.name.clone();
        let repo = self.repo.clone();
        let dir = self.dir.to_string_lossy().into_owned();
        let g = |args: &[&str]| {
            let mut v = vec!["git".to_string(), "-C".into(), dir.clone()];
            v.extend(args.iter().map(|s| s.to_string()));
            v
        };
        let mut items = Vec::new();

        if !self.is_current(&name) {
            let have = self.local.locals.contains(&name);
            items.push(
                Item::new(
                    "Checkout",
                    format!("git switch {name}"),
                    Act::Run(g(&["switch", &name])),
                )
                // It moves a clone onto another branch, and this is launched
                // from anywhere -- so the clone may be one with an editor
                // open in it that nobody is looking at.
                .confirm()
                .note(if have {
                    &["You already have this branch locally."]
                } else {
                    &[
                        "You have no local copy, so git creates one",
                        "tracking origin -- which is why the marker",
                        "column is blank on this row.",
                    ]
                }),
            );
        }
        // Offered on every branch, including one with no lock on it, because
        // "no rules apply" is a different answer from "unprotected by
        // mistake" and only this call can tell them apart.
        items.push(Item::new(
            "Effective rules",
            format!("gh api repos/{repo}/rules/branches/{name}"),
            Act::open({
                let (repo, name, fetch) = (repo.clone(), name.clone(), Arc::clone(&self.fetch));
                move || Box::new(crate::rules::Rules::open(&repo, &name, Arc::clone(&fetch)))
            }),
        ));
        match self.prs.get(&name) {
            Some((n, _)) => {
                items.push(Item::new(
                    format!("Open PR #{n}"),
                    "gh pr view --web",
                    Act::Run(vec![
                        "gh".into(),
                        "pr".into(),
                        "view".into(),
                        n.to_string(),
                        "--repo".into(),
                        repo.clone(),
                        "--web".into(),
                    ]),
                ));
                items.push(Item::new(
                    format!("Diff PR #{n}"),
                    "gh pr diff | less",
                    Act::Run(vec![
                        "gh".into(),
                        "pr".into(),
                        "diff".into(),
                        n.to_string(),
                        "--repo".into(),
                        repo.clone(),
                    ]),
                ));
            }
            None if !self.is_default(&name) => {
                // `gh pr create` with no flags PROMPTS for title and body on a
                // tty, which is why this needs no text input of its own.
                items.push(
                    Item::new(
                        "New pull request",
                        "gh pr create",
                        // `--repo` is load-bearing, not tidiness: without it
                        // `gh pr create` resolves the repo from the process's
                        // cwd, and this TUI is launched from anywhere --
                        // which is the entire point of the repo index. Run
                        // from outside the clone it failed with `not a git
                        // repository`. Verified that `-R` alone is enough: gh
                        // resolves the base branch from the API, so there is
                        // no `--base` to supply.
                        Act::Run(vec![
                            "gh".into(),
                            "pr".into(),
                            "create".into(),
                            "--repo".into(),
                            repo.clone(),
                            "--head".into(),
                            name.clone(),
                        ]),
                    )
                    .note(&[
                        "gh asks for the title and body itself, so this",
                        "needs no text entry of its own.",
                    ]),
                );
            }
            None => {}
        }
        if !self.is_default(&name) {
            // The design's "rename branch", and the first real consumer of
            // `Flow::Ask`. `gh` has no rename command and the API wants the
            // new name as a field, so this is the shape of thing that still
            // needs a text field: a short identifier, not a body.
            items.push(
                Item::new(
                    "Rename branch",
                    "gh api -X POST .../rename",
                    Act::Prompt {
                        label: format!("rename {name} to"),
                        initial: name.clone(),
                        argv: vec![
                            "gh".into(),
                            "api".into(),
                            "-X".into(),
                            "POST".into(),
                            format!("repos/{repo}/branches/{name}/rename"),
                            "-f".into(),
                            "new_name={}".into(),
                        ],
                    },
                )
                // `gh api` neither asks nor reports, and everyone tracking
                // the branch finds out afterwards. Asked after the field, so
                // what is approved has the new name in it.
                .confirm()
                .note(&[
                    "Renames it on the remote and moves any open PR with",
                    "it. Your local branch keeps the old name until you",
                    "fetch and re-point it.",
                ]),
            );
            // No input needed, so no field: `gh` takes the branch as an
            // argument and reports what it did -- afterwards, which is not
            // asking. It changes what every new clone and pull request
            // targets, so it asks here.
            items.push(
                Item::new(
                    "Set as default branch",
                    "gh repo edit --default-branch",
                    Act::Run(vec![
                        "gh".into(),
                        "repo".into(),
                        "edit".into(),
                        repo.clone(),
                        "--default-branch".into(),
                        name.clone(),
                    ]),
                )
                .confirm()
                .note(&["Needs admin on the repo; gh says so if you have not."]),
            );
        }
        items.push(Item::new(
            "Open in browser",
            "gh browse --branch",
            Act::Run(vec![
                "gh".into(),
                "browse".into(),
                "--repo".into(),
                repo.clone(),
                "--branch".into(),
                name.clone(),
            ]),
        ));
        if !self.is_default(&name) {
            items.push(
                Item::new(
                    "Delete remote branch",
                    format!("git push origin --delete {name}"),
                    Act::Run(g(&["push", "origin", "--delete", &name])),
                )
                .danger()
                .note(&[
                    "Deletes it on the remote. Your local copy stays.",
                    "git prints what it did and fails loudly if the",
                    "branch is protected.",
                ]),
            );
        }
        Menu::new("branch", name, items).marks(&self.writes)
    }
}

fn trunc(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        return s.to_string();
    }
    s.chars().take(w.saturating_sub(1)).collect::<String>() + "\u{2026}"
}

impl Screen for Branches {
    fn title(&self) -> String {
        format!("{} \u{b7} branches", self.repo)
    }

    /// The whole point of `Split::Full`: six columns of fact per row, and
    /// nothing a pane could add that the row does not already say.
    fn split(&self) -> Split {
        Split::Full
    }

    fn focus(&mut self, sel: usize) {
        self.sel = sel;
        self.status.clear();
    }

    /// The cursor's own row, which is row 0 on opening and the heading of a
    /// group this screen has just closed from inside. See `Action::Close`.
    fn initial_sel(&self) -> usize {
        self.sel
    }

    fn tick(&mut self) -> Tick {
        // Only the top screen ticks, so what a menu ran is first noticed
        // here once that menu has closed.
        let wrote = self.writes.since(&mut self.wrote_seen);
        if wrote || self.stale {
            self.stale = false;
            self.reread();
            return Tick::Changed;
        }
        if self.fetch.landed(&mut self.epoch_seen) {
            self.sync();
            // "refreshing" until the last answer is in, and not after: the
            // word is in the footer, which nothing else would ever clear.
            if !self.fetch.pending() {
                self.status.clear();
            }
            return Tick::Changed;
        }
        self.fetch.waiting()
    }

    fn rows(&mut self) -> Vec<Row> {
        if self.groups.is_empty() {
            // A refusal first: it is neither still loading nor an empty repo,
            // and with something else in flight it would be drawn as the one,
            // and afterwards as the other.
            let nothing_local = || "nothing under refs/remotes/origin".to_string();
            let (label, detail) = match &self.unknown.branches {
                Some(e) => ("could not list branches", e.clone()),
                None if self.fetch.pending() => ("asking github\u{2026}", nothing_local()),
                None => ("no branches found", nothing_local()),
            };
            return vec![Row::leaf(label).detail(detail)];
        }
        let mut rows = Vec::new();
        for (g, (_, name, ids)) in self.groups.iter().enumerate() {
            let mut head = Row::leaf(*name);
            head.group = true;
            head.expanded = self.open[g];
            rows.push(head.detail(match ids.len() {
                1 => "1".to_string(),
                n => n.to_string(),
            }));
            if self.open[g] {
                for &i in ids {
                    rows.push(self.row_for(&self.branches[i]));
                }
            }
            rows.push(Row::spacer());
        }
        rows
    }

    /// Never drawn -- `Split::Full` means `app` does not ask. Implemented
    /// because the trait requires it, and kept honest rather than
    /// `unreachable!()` in case a caller ever composes this differently.
    fn pane(&mut self, _sel: usize, _cols: u16, _rows: u16) -> Pane {
        Pane::default()
    }

    fn on_action(&mut self, action: Action, sel: usize) -> Flow {
        self.status.clear();
        match (action, self.at(sel)) {
            (Action::Activate, At::Head(g)) => {
                self.open[g] = !self.open[g];
                Flow::Dirty
            }
            // `l` is a direction: it opens what is closed and otherwise moves
            // on. Toggling made it a second way to CLOSE a group, so leaning
            // on it flickered the group open and shut.
            (Action::Open, At::Head(g)) if !self.open[g] => {
                self.open[g] = true;
                Flow::Dirty
            }
            (Action::Close, At::Head(g)) => {
                self.open[g] = false;
                Flow::Dirty
            }
            // Closing a group from one of its branches takes the cursor to
            // the heading. Left at its old index it sat on whatever moved up
            // into that row -- the next group's heading, or one of its
            // branches -- and the next `enter` opened a menu for a branch
            // nobody had been looking at.
            (Action::Close, At::Branch(g, _)) => {
                self.open[g] = false;
                self.sel = self.head_row(g);
                Flow::Retarget
            }
            // The three-dots. Enter on a branch opens everything you can do to
            // it, named, rather than hiding it behind keys you have to be told.
            // `Activate` only -- `l` is a direction.
            (Action::Activate, At::Branch(..)) => {
                let Some(menu) = self.branch_at(sel).map(|b| self.menu_for(b)) else {
                    return Flow::Continue;
                };
                // Anything it runs may change what this screen shows, and
                // the menu counts each one: see `tick`.
                Flow::Push(Box::new(menu))
            }
            (Action::Key('r'), _) => {
                self.reread();
                self.status = "refreshing\u{2026}".into();
                Flow::Dirty
            }
            (Action::Key('f'), _) => {
                // A real fetch, on the real terminal: it can prompt for a key
                // passphrase, and it is the thing that makes ahead/behind true
                // again.
                self.stale = true;
                Flow::Detach(vec![
                    "git".into(),
                    "-C".into(),
                    self.dir.to_string_lossy().into_owned(),
                    "fetch".into(),
                    "--prune".into(),
                    "origin".into(),
                ])
            }
            _ => Flow::Continue,
        }
    }

    fn footer(&self) -> String {
        let legend = format!(
            "{DIM}\u{25cf} here  \u{25cb} local copy  \u{2b50} default  \u{1f512} protected  \u{2191}ahead \u{2193}behind the default{OFF}"
        );
        let mut keys = match self.at(self.sel) {
            At::Head(_) => "enter open/close \u{b7} f fetch \u{b7} r refresh",
            At::Branch(..) => "enter what you can do to it \u{b7} f fetch \u{b7} r refresh",
            At::Nothing => "f fetch \u{b7} r refresh",
        }
        .to_string();
        // On the keys' own line, so the footer does not grow and shrink by a
        // row around every refresh. The footer because there is no pane: the
        // word used to be set and shown nowhere.
        if !self.status.is_empty() {
            keys.push_str(&format!(" \u{b7} {ORG}{}{OFF}", self.status));
        }
        let mut lines = self.notices();
        lines.push(keys);
        lines.push(legend);
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menu::check;

    fn screen() -> Branches {
        let mut b = Branches {
            repo: "roest1/dotfiles".into(),
            dir: std::path::PathBuf::from("/nonexistent"),
            fetch: Arc::new(Fetcher::new("roest1/dotfiles")),
            local: Local {
                default: Some("main".into()),
                current: Some("feat/x".into()),
                // feat/x is checked out, feat/y is fetched but not checked
                // out, and chore/z has never been fetched -- one row per
                // marker state.
                locals: std::collections::HashSet::from([
                    "feat/x".to_string(),
                    "feat/y".to_string(),
                ]),
                me_email: Some("me@example.com".into()),
                me_name: Some("Riley Oest".into()),
                facts: HashMap::new(),
            },
            api: None,
            branches: Vec::new(),
            groups: Vec::new(),
            open: Vec::new(),
            prs: HashMap::new(),
            prs_cut: false,
            checks: HashMap::new(),
            epoch_seen: 0,
            unknown: Unknown::default(),
            status: String::new(),
            sel: 0,
            stale: false,
            writes: Writes::default(),
            wrote_seen: 0,
        };
        // Two of mine, authored under DIFFERENT names but one email -- the
        // real shape on this machine, and what makes name-matching wrong.
        for (name, who, email, ab) in [
            ("main", "Someone Else", "other@example.com", (0u32, 0u32)),
            ("feat/x", "Riley Oest", "me@example.com", (3, 1)),
            ("feat/y", "roest1", "me@example.com", (1, 0)),
            ("chore/z", "Someone Else", "other@example.com", (2, 7)),
        ] {
            b.local.facts.insert(
                name.into(),
                Facts {
                    when: 1_700_000_000,
                    who: who.into(),
                    email: email.into(),
                    subject: format!("work on {name}"),
                    ahead_behind: Some(ab),
                },
            );
        }
        b.rebuild();
        b
    }

    /// The three groups the design asks for, with the default on its own and
    /// "all" genuinely repeating "yours" -- which is what github.com does and
    /// what makes "yours" a shortcut rather than a filter.
    #[test]
    fn it_groups_default_yours_and_all() {
        let b = screen();
        let names: Vec<String> = b
            .groups
            .iter()
            .map(|(_, n, _)| n.split_whitespace().skip(1).collect::<Vec<_>>().join(" "))
            .collect();
        assert_eq!(names, vec!["default", "your branches", "all branches"]);
        assert_eq!(b.groups[0].2.len(), 1, "default is alone");
        // Both of mine, despite the two different author NAMES.
        assert_eq!(b.groups[1].2.len(), 2, "two of mine");
        assert_eq!(b.groups[2].2.len(), 3, "all, minus the default");
    }

    /// The invariant every tree screen here carries.
    #[test]
    fn the_walk_matches_the_rows() {
        let mut b = screen();
        for shape in [
            vec![true, true, true],
            vec![true, false, true],
            vec![false; 3],
        ] {
            b.open = shape;
            let rows = b.rows();
            for (i, row) in rows.iter().enumerate() {
                if !row.selectable {
                    continue;
                }
                match b.at(i) {
                    At::Head(g) => assert_eq!(row.label, b.groups[g].1),
                    At::Branch(g, k) => {
                        let want = &b.branches[b.groups[g].2[k]].name;
                        assert!(row.label.contains(want.as_str()), "{} vs {want}", row.label);
                    }
                    At::Nothing => panic!("row {i} ({}) resolves to nothing", row.label),
                }
            }
        }
    }

    /// `app` packs badges to the right but does not align them, so the screen
    /// pads each to a fixed width. If that slips, the table stops being a
    /// table -- and it is invisible in a unit test that only reads content.
    #[test]
    fn every_branch_row_has_the_same_column_widths() {
        let mut b = screen();
        b.open = vec![true, true, true];
        let rows = b.rows();
        let branch_rows: Vec<&Row> = rows
            .iter()
            .enumerate()
            .filter(|(i, r)| r.selectable && matches!(b.at(*i), At::Branch(..)))
            .map(|(_, r)| r)
            .collect();
        assert!(branch_rows.len() >= 4);
        let widths: Vec<Vec<usize>> = branch_rows
            .iter()
            .map(|r| r.badges.iter().map(|(t, _)| t.chars().count()).collect())
            .collect();
        for w in &widths {
            assert_eq!(*w, widths[0], "columns differ across rows");
        }
    }

    /// The default branch is what the others are measured against, so two
    /// zeros would read as a fact it does not have.
    #[test]
    fn the_default_branch_shows_no_ahead_behind() {
        let mut b = screen();
        b.open = vec![true, true, true];
        let rows = b.rows();
        let main = rows
            .iter()
            .find(|r| r.label.contains("main"))
            .expect("main");
        let ab = &main.badges[2].0;
        assert!(ab.trim().is_empty(), "default shows {ab:?}");
        // And a non-default one does carry it.
        let x = rows
            .iter()
            .find(|r| r.label.contains("feat/x"))
            .expect("feat/x");
        assert!(x.badges[2].0.contains('\u{2191}'), "{:?}", x.badges[2].0);
    }

    /// The marker column is two cells whether or not anything is in it, or
    /// names would start in different columns down the list.
    #[test]
    fn the_marker_column_is_fixed_width() {
        let mut b = screen();
        b.open = vec![true, true, true];
        let rows = b.rows();
        // Counted in CELLS. This assertion has now been wrong twice in two
        // different ways, which is why it is written out rather than inlined:
        // `str::find` returns a BYTE offset and `\u{2b50}` is four bytes, and
        // `chars().count()` returns CHARACTERS where a star is one char but
        // two cells. Both versions failed on a perfectly aligned row, and both
        // would have passed on a broken one whose counts happened to cancel.
        // Only cells describe what the terminal does.
        const MARKS: [char; 5] = [' ', '\u{25cf}', '\u{25cb}', '\u{2b50}', '\u{1f512}'];
        for (i, r) in rows.iter().enumerate() {
            if !matches!(b.at(i), At::Branch(..)) {
                continue;
            }
            let marks: String = r.label.chars().take_while(|c| MARKS.contains(c)).collect();
            assert_eq!(
                tui_core::width::width(&marks),
                4,
                "marker prefix of {:?} is {} cells, so its name is in the wrong column",
                r.label,
                tui_core::width::width(&marks)
            );
        }
    }

    /// Three states in one cell: on it, have it, or remote only. The last is
    /// the one the table could not say before, and it is the one that decides
    /// whether Checkout creates a branch or moves to one.
    #[test]
    fn the_marker_says_whether_you_have_a_local_copy() {
        let mut b = screen();
        b.open = vec![true, true, true];
        let rows = b.rows();
        let mark = |name: &str| -> char {
            rows.iter()
                .find(|r| r.label.contains(name))
                .unwrap_or_else(|| panic!("no row for {name}"))
                .label
                .chars()
                .next()
                .unwrap()
        };
        assert_eq!(mark("feat/x"), '\u{25cf}', "checked out");
        assert_eq!(mark("feat/y"), '\u{25cb}', "fetched, not checked out");
        assert_eq!(mark("chore/z"), ' ', "remote only");

        // And the menu says which of the two Checkout is about to do, since
        // creating a branch and moving to one are different acts.
        let z = b
            .branches
            .iter()
            .find(|x| x.name == "chore/z")
            .unwrap()
            .clone();
        let pane = b.menu_for(&z).pane(0, 60, 40).lines.join("\n");
        assert!(pane.contains("no local copy"), "{pane}");
        let y = b
            .branches
            .iter()
            .find(|x| x.name == "feat/y")
            .unwrap()
            .clone();
        let pane = b.menu_for(&y).pane(0, 60, 40).lines.join("\n");
        assert!(pane.contains("already have"), "{pane}");
    }

    /// Effective rules is offered on EVERY branch, lock or no lock: "no rules
    /// apply" and "unprotected by mistake" are different answers, and only
    /// the call can tell them apart.
    #[test]
    fn every_branch_can_be_asked_what_actually_rules_it() {
        let mut b = screen();
        b.open = vec![true, true, true];
        for name in ["main", "feat/x", "chore/z"] {
            let br = b.branches.iter().find(|x| x.name == name).unwrap().clone();
            let labels: Vec<String> = b
                .menu_for(&br)
                .rows()
                .iter()
                .map(|r| r.label.clone())
                .collect();
            assert!(
                labels.iter().any(|l| l == "Effective rules"),
                "{name}: {labels:?}"
            );
        }
    }

    /// Nothing a menu offers may depend on the process's working directory.
    ///
    /// This TUI is launched from anywhere -- that is what the repo index is
    /// for -- so a `gh` command has to name the repo and a `git` command has
    /// to carry `-C`. `New pull request` did neither: `gh pr create --head x`
    /// resolves the repo from the cwd, so it worked when run inside the clone
    /// and failed with `not a git repository` everywhere else. Found by
    /// driving it, which is also the only way it could have been found.
    #[test]
    fn no_menu_item_depends_on_the_working_directory() {
        let mut b = screen();
        b.open = vec![true, true, true];
        for name in ["main", "feat/x", "feat/y", "chore/z"] {
            let br = b.branches.iter().find(|x| x.name == name).unwrap().clone();
            for item in b.menu_for(&br).items() {
                let Some(argv) = check::argv_of(item) else {
                    continue;
                };
                assert!(
                    check::names_repo(argv, "roest1/dotfiles"),
                    "{name}: {:?} names no repo, or runs git without -C: {argv:?}",
                    item.label
                );
            }
        }
    }

    /// The menu is built from what is TRUE of that branch: no checkout for the
    /// one you are on, no delete for the default, and "new PR" only when there
    /// is not already one.
    #[test]
    fn the_menu_offers_only_what_applies() {
        let mut b = screen();
        let labels =
            |m: &mut Menu| -> Vec<String> { m.rows().iter().map(|r| r.label.clone()).collect() };

        let main = b
            .branches
            .iter()
            .find(|x| x.name == "main")
            .unwrap()
            .clone();
        let l = labels(&mut b.menu_for(&main));
        assert!(!l.iter().any(|s| s.contains("Delete")), "default: {l:?}");
        assert!(!l.iter().any(|s| s.contains("New pull")), "default: {l:?}");

        let cur = b
            .branches
            .iter()
            .find(|x| x.name == "feat/x")
            .unwrap()
            .clone();
        let l = labels(&mut b.menu_for(&cur));
        assert!(!l.iter().any(|s| s.contains("Checkout")), "current: {l:?}");
        assert!(l.iter().any(|s| s.contains("New pull")), "{l:?}");
        assert!(l.iter().any(|s| s.contains("Delete")), "{l:?}");

        b.prs.insert("feat/y".into(), (42, false));
        let other = b
            .branches
            .iter()
            .find(|x| x.name == "feat/y")
            .unwrap()
            .clone();
        let l = labels(&mut b.menu_for(&other));
        assert!(l.iter().any(|s| s.contains("Checkout")), "{l:?}");
        assert!(l.iter().any(|s| s.contains("#42")), "{l:?}");
        assert!(!l.iter().any(|s| s.contains("New pull")), "has one: {l:?}");
    }

    /// Git < 2.41 has no `%(ahead-behind:)`. The table must still draw, with
    /// those two columns blank rather than the screen failing.
    #[test]
    fn an_old_git_loses_two_columns_and_nothing_else() {
        let mut b = screen();
        for f in b.local.facts.values_mut() {
            f.ahead_behind = None;
        }
        b.branches.clear();
        b.rebuild();
        b.open = vec![true, true, true];
        let rows = b.rows();
        let x = rows
            .iter()
            .find(|r| r.label.contains("feat/x"))
            .expect("feat/x");
        assert!(x.badges[2].0.trim().is_empty());
        // The author and subject still arrive, which is the point.
        assert!(x.detail.as_deref().unwrap_or("").contains("work on"));
    }

    /// Put GitHub's answer to one of this screen's questions in by hand.
    fn answer(b: &Branches, tail: &str, v: serde_json::Value) {
        b.fetch.seed(b.req(tail), Got::Json(Arc::new(v)));
    }

    fn listing(names: &[&str]) -> serde_json::Value {
        names
            .iter()
            .map(|n| serde_json::json!({"name": n, "protected": *n == "main"}))
            .collect()
    }

    /// What `read_local` would return now, to change and hand back.
    fn reading(b: &Branches) -> Local {
        Local {
            default: b.local.default.clone(),
            current: b.local.current.clone(),
            locals: b.local.locals.clone(),
            me_email: b.local.me_email.clone(),
            me_name: b.local.me_name.clone(),
            facts: b.local.facts.clone(),
        }
    }

    fn names(b: &Branches) -> Vec<&str> {
        let mut n: Vec<&str> = b.branches.iter().map(|x| x.name.as_str()).collect();
        n.sort_unstable();
        n
    }

    /// A menu item that changes something asks first, asserted over every item
    /// of every branch's menu. Written as the short list of commands that only
    /// READ, so an item added later has to ask unless someone says here that
    /// it need not -- the safe way round for a rule about deletes.
    #[test]
    fn everything_that_changes_something_asks_first() {
        let mut b = screen();
        b.open = vec![true, true, true];
        b.prs.insert("feat/y".into(), (42, false));
        let mut asking: Vec<String> = Vec::new();
        for name in ["main", "feat/x", "feat/y", "chore/z"] {
            let br = b.branches.iter().find(|x| x.name == name).unwrap().clone();
            for item in b.menu_for(&br).items() {
                let Some(argv) = check::argv_of(item) else {
                    continue;
                };
                match check::kind(argv) {
                    check::Kind::Reads => {}
                    // The one writer here that stops and asks for itself: gh
                    // wants a title and a body before it creates anything,
                    // and a second prompt in front of that is one too many.
                    check::Kind::GhAsks => {
                        assert!(!item.asks, "{name}: {:?} asks twice", item.label)
                    }
                    check::Kind::Writes => {
                        assert!(
                            item.asks,
                            "{name}: {:?} runs {argv:?} on one keypress",
                            item.label
                        );
                        asking.push(item.label.clone());
                    }
                }
            }
        }
        // And the list above did not quietly swallow the ones that matter.
        for label in [
            "Checkout",
            "Rename branch",
            "Set as default branch",
            "Delete remote branch",
        ] {
            assert!(asking.iter().any(|l| l == label), "{label} was never seen");
        }
    }

    /// The table is what git and GitHub say NOW. It used to be added to and
    /// never taken from, with each row keeping the facts it was first given.
    #[test]
    fn the_table_follows_a_delete_a_rename_and_a_fetch() {
        let mut b = screen();
        b.open = vec![true, true, true];

        // A fetch: the same branch, with a newer commit on it.
        let mut now = reading(&b);
        let x = now.facts.get_mut("feat/x").unwrap();
        x.subject = "a newer commit".into();
        x.ahead_behind = Some((9, 0));
        b.adopt(now);
        let rows = b.rows();
        let x = rows.iter().find(|r| r.label.contains("feat/x")).unwrap();
        assert_eq!(x.detail.as_deref(), Some("a newer commit"));
        assert!(x.badges[2].0.contains('9'), "{:?}", x.badges[2].0);

        // A delete: `git push --delete` drops the remote-tracking ref too.
        let mut now = reading(&b);
        now.facts.remove("chore/z");
        b.adopt(now);
        assert_eq!(names(&b), ["feat/x", "feat/y", "main"]);

        // A rename happens on GitHub alone, so git still has the old ref and
        // not the new one. GitHub's whole list is what says which exists.
        answer(&b, BRANCHES, listing(&["main", "feat/x", "feat/renamed"]));
        b.sync();
        assert_eq!(names(&b), ["feat/renamed", "feat/x", "main"]);
        b.open = vec![true; b.groups.len()];
        let rows = b.rows();
        let renamed = rows
            .iter()
            .find(|r| r.label.contains("feat/renamed"))
            .unwrap();
        assert_eq!(renamed.detail.as_deref(), Some("not fetched yet"));
        assert!(!rows.iter().any(|r| r.label.contains("feat/y")));
    }

    /// One page is a hundred. A full one may have been cut, so it cannot say
    /// which branches do NOT exist -- and the footer says the list is short,
    /// because past the cut a missing lock is not a fact.
    #[test]
    fn a_cut_branch_list_drops_nothing_and_says_it_was_cut() {
        let page: Vec<String> = (0..PAGE).map(|i| format!("b{i:03}")).collect();
        let page: Vec<&str> = page.iter().map(String::as_str).collect();

        let mut b = screen();
        answer(&b, BRANCHES, listing(&page));
        b.sync();
        assert_eq!(b.branches.len(), PAGE + 4, "git's own four were dropped");
        assert!(b.footer().contains("first 100 branches"), "{}", b.footer());

        // One short of a page is the whole list, and then it does decide.
        let mut b = screen();
        answer(&b, BRANCHES, listing(&page[1..]));
        b.sync();
        assert_eq!(b.branches.len(), PAGE - 1);
        assert!(!b.footer().contains("first 100"), "{}", b.footer());
    }

    /// A refusal is said out loud WITH the table on screen. The table draws
    /// from git either way, so a denied branch list used to look exactly like
    /// a repository with nothing protected.
    #[test]
    fn what_github_would_not_say_is_named_not_left_blank() {
        let mut b = screen();
        answer(&b, BRANCHES, listing(&["main", "feat/x"]));
        b.sync();
        assert!(b.branches.iter().any(|x| x.protected));
        assert!(!b.footer().contains("unknown"), "{}", b.footer());

        b.fetch.seed(
            b.req(BRANCHES),
            Got::Failed("gh: Not Found (HTTP 404)".into()),
        );
        b.fetch.seed(
            b.req(PULLS),
            Got::Denied(crate::scopes::Denied {
                accepted: vec!["repo".into()],
                suggest: "repo".into(),
            }),
        );
        b.prs.insert("feat/x".into(), (7, false));
        b.sync();
        // Git's table is still there, without last time's locks or numbers...
        assert_eq!(names(&b), ["chore/z", "feat/x", "feat/y", "main"]);
        assert!(!b.branches.iter().any(|x| x.protected));
        assert!(b.prs.is_empty());
        // ...and each gap has its own reason beside it.
        let foot = b.footer();
        assert!(foot.contains("protection unknown"), "{foot}");
        assert!(foot.contains("HTTP 404"), "{foot}");
        assert!(foot.contains("pull requests unknown"), "{foot}");
        assert!(foot.contains("gh auth refresh"), "{foot}");

        // Offline, everything fails the same way: one line, not three.
        for tail in [BRANCHES, PULLS, RUNS] {
            b.fetch.seed(b.req(tail), Got::Failed("no network".into()));
        }
        b.sync();
        let foot = b.footer();
        assert!(
            foot.contains("protection, pull requests and checks unknown"),
            "{foot}"
        );
        assert_eq!(foot.matches("no network").count(), 1, "{foot}");
    }

    /// The row drawn in place of the table is a row, and it is nothing: no key
    /// acts on a branch that is not there. And a failure is not drawn as
    /// loading, nor as an empty repository.
    #[test]
    fn the_walk_matches_the_rows_when_there_is_no_table() {
        let mut b = screen();
        b.adopt(Local {
            facts: HashMap::new(),
            ..reading(&b)
        });
        let mut labels = Vec::new();
        for failed in [false, true] {
            if failed {
                b.fetch
                    .seed(b.req(BRANCHES), Got::Failed("gh: HTTP 502".into()));
                b.sync();
            }
            let rows = b.rows();
            assert_eq!(rows.len(), 1);
            assert!(matches!(b.at(0), At::Nothing));
            for a in [Action::Activate, Action::Open, Action::Close] {
                assert!(matches!(b.on_action(a, 0), Flow::Continue), "{a:?} acted");
            }
            labels.push((rows[0].label.clone(), rows[0].detail.clone()));
        }
        assert_eq!(labels[0].0, "no branches found");
        assert_eq!(labels[1].0, "could not list branches");
        assert_eq!(labels[1].1.as_deref(), Some("gh: HTTP 502"));
    }

    /// A ref name is not a branch of THIS repository. A fork's pull request
    /// from its own `main` put a PR number on this `main` and took "New pull
    /// request" off its menu, and the fork's failing run coloured its checks.
    #[test]
    fn a_forks_branch_of_the_same_name_is_not_this_one() {
        let mut b = screen();
        let (here, fork) = (serde_json::json!({"id": 1}), serde_json::json!({"id": 2}));
        answer(
            &b,
            PULLS,
            serde_json::json!([
                {"number": 9, "head": {"ref": "feat/x", "repo": fork}, "base": {"ref": "main", "repo": here}},
                {"number": 8, "head": {"ref": "chore/z", "repo": null}, "base": {"ref": "main", "repo": here}},
                {"number": 7, "draft": true, "head": {"ref": "feat/y", "repo": here}, "base": {"ref": "main", "repo": here}},
            ]),
        );
        answer(
            &b,
            RUNS,
            serde_json::json!({"workflow_runs": [
                {"head_branch": "main", "status": "completed", "conclusion": "failure",
                 "head_repository": fork, "repository": here},
                {"head_branch": "main", "status": "completed", "conclusion": "success",
                 "head_repository": here, "repository": here},
                {"head_branch": "feat/x", "status": "in_progress", "conclusion": null,
                 "head_repository": null, "repository": here},
            ]}),
        );
        b.sync();
        assert_eq!(b.prs.len(), 1, "{:?}", b.prs);
        assert_eq!(b.prs.get("feat/y"), Some(&(7, true)));
        assert_eq!(b.checks.len(), 1, "{:?}", b.checks);
        assert_eq!(b.checks.get("main"), Some(&Verdict::Passed));

        let x = b
            .branches
            .iter()
            .find(|x| x.name == "feat/x")
            .unwrap()
            .clone();
        let labels: Vec<String> = b.menu_for(&x).rows().into_iter().map(|r| r.label).collect();
        assert!(labels.iter().any(|l| l == "New pull request"), "{labels:?}");
    }

    /// A branch name may hold slashes, and only the remote's prefix is not
    /// part of it. The last segment alone named a branch that does not exist,
    /// so nothing was the default and the real one could be deleted.
    #[test]
    fn a_default_branch_with_a_slash_in_it_keeps_its_whole_name() {
        assert_eq!(
            default_of("refs/remotes/origin/release/v2").as_deref(),
            Some("release/v2")
        );
        assert_eq!(
            default_of("refs/remotes/origin/main").as_deref(),
            Some("main")
        );
        assert_eq!(default_of("refs/remotes/origin/"), None);
        assert_eq!(default_of("refs/heads/main"), None);

        let mut b = screen();
        let mut now = reading(&b);
        now.default = default_of("refs/remotes/origin/feat/x");
        b.adopt(now);
        assert!(matches!(b.groups[0].0, Group::Default));
        let x = b.branches[b.groups[0].2[0]].clone();
        assert_eq!(x.name, "feat/x");
        let labels: Vec<String> = b.menu_for(&x).rows().into_iter().map(|r| r.label).collect();
        assert!(
            !labels
                .iter()
                .any(|l| l.contains("Delete") || l.contains("Rename")),
            "{labels:?}"
        );
    }

    /// `h` on a branch closes its group and the cursor goes to the heading.
    /// Its old index is by then some other row: from the second of your
    /// branches it was the heading of the group BELOW.
    #[test]
    fn closing_a_group_from_inside_takes_the_cursor_to_its_heading() {
        let mut b = screen();
        b.open = vec![true, true, true];
        let rows = b.rows();
        for (i, row) in rows.iter().enumerate() {
            b.open = vec![true, true, true];
            let At::Branch(g, _) = b.at(i) else { continue };
            b.focus(i);
            assert!(
                matches!(b.on_action(Action::Close, i), Flow::Retarget),
                "{} did not move the cursor",
                row.label
            );
            assert!(!b.open[g]);
            let landed = b.initial_sel();
            assert!(matches!(b.at(landed), At::Head(h) if h == g));
            assert_eq!(b.rows()[landed].label, b.groups[g].1, "from {}", row.label);
        }
    }

    /// `l` opens a closed group and does nothing else. `enter` is the toggle.
    #[test]
    fn l_opens_a_group_and_never_closes_one() {
        let mut b = screen();
        b.open = vec![false, false, false];
        assert!(matches!(b.on_action(Action::Open, 0), Flow::Dirty));
        assert!(b.open[0]);
        assert!(matches!(b.on_action(Action::Open, 0), Flow::Continue));
        assert!(b.open[0], "a second l closed it again");
        // And on a branch it is a direction, never the menu.
        assert!(matches!(b.at(1), At::Branch(..)));
        assert!(matches!(b.on_action(Action::Open, 1), Flow::Continue));
        assert!(matches!(b.on_action(Action::Activate, 0), Flow::Dirty));
        assert!(!b.open[0]);
    }

    /// The three answers a tick can give, in the order that matters: something
    /// landed, something is in flight, nothing is. And "refreshing" is on
    /// screen from `r` until the answers are in, where it used to be set and
    /// drawn nowhere.
    #[test]
    fn a_refresh_is_shown_awaited_and_then_forgotten() {
        let mut b = screen();
        assert!(matches!(b.tick(), Tick::Idle));

        b.status = "refreshing\u{2026}".into();
        assert!(b.footer().contains("refreshing"), "{}", b.footer());
        // Something landed: the epoch is not the one last seen.
        b.epoch_seen = u64::MAX;
        assert!(matches!(b.tick(), Tick::Changed));
        assert!(!b.footer().contains("refreshing"), "{}", b.footer());
        assert!(matches!(b.tick(), Tick::Idle));

        // In flight, with the old answer still on show. Never idle: that is
        // the screen sitting on the old table until a key is pressed. A local
        // usage error, so nothing leaves the machine.
        b.fetch.refresh(Req::Cmd {
            args: vec!["--definitely-not-a-gh-flag".into()],
        });
        assert!(matches!(b.tick(), Tick::Wake(_) | Tick::Changed));
    }

    /// The row a branch is drawn on, with every group open.
    fn row_of(b: &mut Branches, name: &str) -> usize {
        b.open = vec![true, true, true];
        (0..b.rows().len())
            .find(|&sel| b.branch_at(sel).is_some_and(|x| x.name == name))
            .expect("the branch is in the table")
    }

    /// A menu that ran something is followed by a re-read: the branch it
    /// deleted or renamed is otherwise still in the table.
    #[test]
    fn a_menu_that_ran_something_makes_the_table_ask_again() {
        let mut b = screen();
        let row = row_of(&mut b, "chore/z");
        let Flow::Push(mut menu) = b.on_action(Action::Activate, row) else {
            panic!("enter on a branch opens its menu");
        };
        assert_eq!(menu.rows()[0].label, "Checkout");
        // It asks first, and asking is not running.
        assert!(matches!(menu.on_action(Action::Activate, 0), Flow::Dirty));
        assert!(!matches!(b.tick(), Tick::Changed), "re-read before a `y`");
        assert!(matches!(
            menu.on_action(Action::Key('y'), 0),
            Flow::Detach(_)
        ));
        assert!(matches!(b.tick(), Tick::Changed));
        assert!(
            b.fetch.get(&b.req(BRANCHES)).is_some(),
            "github was not asked again"
        );
        assert!(
            !matches!(b.tick(), Tick::Changed),
            "asked once, not on every tick"
        );
    }

    /// And one that was only looked at is not. The screen used to call itself
    /// stale as the menu opened, so every look at a branch's menu re-read git
    /// and asked GitHub three things on the way back.
    #[test]
    fn a_menu_that_was_only_looked_at_asks_for_nothing() {
        let mut b = screen();
        let row = row_of(&mut b, "chore/z");
        assert!(matches!(b.on_action(Action::Activate, row), Flow::Push(_)));
        assert!(!b.stale);
        // The menu's own rules screen is on its way, so this is not idle.
        // What it must not be is a re-read.
        assert!(!matches!(b.tick(), Tick::Changed));
        assert!(
            b.fetch.get(&b.req(BRANCHES)).is_none(),
            "asked for a menu nothing was run from"
        );
    }
}
