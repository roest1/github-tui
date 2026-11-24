//! Pull requests: filters on the left, the list on the right.
//!
//! The first `Split::Sidebar` screen, and the one the mock in `GITHUB_TUI.md`
//! is drawn for. The layout is the inverse of every other screen here -- the
//! left column holds CONTROLS and the content is on the right -- which is why
//! the sidebar needed to be a navigable region rather than a width.
//!
//! ## Every control is a dropdown, and that is deliberate
//!
//! The mock draws the state toggles, the sort order and the search in a header
//! strip above the list, and the five user/label filters down the side. They
//! are all the same thing: a named thing you open, pick from, and close. So
//! they are all rows in the sidebar, one control per row, and `enter` on a row
//! means exactly one thing.
//!
//! The alternative was a header row holding four controls side by side, which
//! needs column-wise focus inside a row -- a second navigation axis, and the
//! one thing the directional model is built to avoid. Riley's note that the
//! sidebar dropdowns and the main-pane dropdowns are the same kind of thing is
//! what made this the obvious shape.
//!
//! ## Selection and bulk actions
//!
//! `space` ticks a pull request, the same key that toggles a section in
//! `dots`. The row at the top of the list says how many are ticked, and with
//! anything ticked `enter` on it opens a menu whose items act on the set. So
//! bulk actions need no extra region and no extra keys: they are the same
//! `Menu` every other screen uses.
//!
//! That row is ALWAYS there, ticked or not. It used to appear with the first
//! tick and go with the last, which moved every pull request down or up a row
//! under a cursor that stayed where it was -- so the second `space` landed on
//! a different pull request from the first.
//!
//! ## What is filtered where
//!
//! `state` goes into the `gh pr list` call, because it changes which pull
//! requests exist. Everything else -- author, assignee, involvement, review
//! requests, labels, sort, and the `/` search -- is applied to the fifty that
//! came back, which is instant and needs no round trip. The user lists are
//! derived from those fifty rather than fetched: the people who have pull
//! requests here are the people worth filtering by, and it saves two
//! endpoints.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use tui_core::{nav::Action, Flow, Pane, Row, Screen, Split, Tick};

use crate::gh::{Fetcher, Got, Req};
use crate::menu::{Act, Item, Menu, Writes};
use crate::paint::{idx, DIM, OFF};
use crate::time;

/// Run the command that follows once for each number in the first argument,
/// with the number appended, and fail if any of them did.
///
/// `gh pr close`, `reopen` and `edit` take exactly ONE pull request: handed
/// two they refuse both and change nothing. A menu item is one command, so
/// the loop has to be inside it. The numbers arrive as an
/// argument and so does whatever was typed into a field, which is why this
/// must never contain `{}` -- `Menu` fills that in wherever it finds it.
const EACH: &str = r#"s=0; for n in $0; do "$@" "$n" || s=1; done; exit $s"#;

/// Everything `gh pr list` is asked for, in one call.
const FIELDS: &str = "number,title,author,headRefName,headRefOid,baseRefName,reviewDecision,\
isDraft,createdAt,updatedAt,additions,deletions,labels,statusCheckRollup,url,state,assignees";

#[derive(Debug, Clone, Deserialize)]
struct Who {
    #[serde(default)]
    login: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Label {
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Check {
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pr {
    number: u64,
    title: String,
    #[serde(default)]
    author: Option<Who>,
    #[serde(default)]
    assignees: Vec<Who>,
    head_ref_name: Option<String>,
    /// The head COMMIT, which is what the runs for a pull request are keyed
    /// on: `GET /actions/runs?head_sha=` takes a sha, and a branch name would
    /// answer for whatever is on the branch NOW rather than for what was
    /// reviewed.
    ///
    /// `Option` is defensive rather than observed -- merged and closed pull
    /// requests were both checked and still carry it, deleted head branch and
    /// all. The menu simply does not offer the row when it is missing, which
    /// is cheaper than finding out from a query for a commit called `None`.
    head_ref_oid: Option<String>,
    base_ref_name: Option<String>,
    review_decision: Option<String>,
    #[serde(default)]
    is_draft: bool,
    created_at: Option<String>,
    updated_at: Option<String>,
    #[serde(default)]
    additions: i64,
    #[serde(default)]
    deletions: i64,
    #[serde(default)]
    labels: Vec<Label>,
    #[serde(default)]
    status_check_rollup: Vec<Check>,
    state: Option<String>,
}

impl Pr {
    fn author_login(&self) -> &str {
        self.author
            .as_ref()
            .map(|a| a.login.as_str())
            .unwrap_or("?")
    }

    /// The review glyph, in the vocabulary the fzf list used.
    fn review(&self) -> (&'static str, u8) {
        match self.state.as_deref() {
            Some("MERGED") => return ("\u{1f7e3} merged", idx::GRY),
            Some("CLOSED") => return ("\u{26d4} closed", idx::GRY),
            _ => {}
        }
        if self.is_draft {
            return ("\u{1f4dd} draft", idx::DIM);
        }
        match self.review_decision.as_deref() {
            Some("APPROVED") => ("\u{2705} approved", idx::GRN),
            Some("CHANGES_REQUESTED") => ("\u{1f504} changes", idx::RED),
            Some("REVIEW_REQUIRED") => ("\u{1f440} review", idx::YEL),
            _ => ("\u{23f3} open", idx::YEL),
        }
    }

    /// The check rollup, collapsed the way the bash `ci` helper did: any
    /// failure is a failure, anything outstanding is outstanding, else green.
    ///
    /// Green is the claim that has to be earned. It used to be what was left
    /// once `FAILURE` and the pending states were ruled out, so a check that
    /// was cancelled, timed out or never started drew a green tick beside a
    /// pull request that could not merge. Now a check counts as passed only
    /// when it says so, and one that says something this does not know is
    /// outstanding rather than fine.
    fn checks(&self) -> Option<(&'static str, u8)> {
        if self.status_check_rollup.is_empty() {
            return None;
        }
        let bad = |c: &Check| {
            matches!(
                c.conclusion.as_deref(),
                Some("FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE")
            ) || matches!(c.state.as_deref(), Some("FAILURE" | "ERROR"))
        };
        // A check run reports a `conclusion`, a commit status a `state`.
        let good = |c: &Check| {
            matches!(
                c.conclusion.as_deref(),
                Some("SUCCESS" | "NEUTRAL" | "SKIPPED")
            ) || matches!(c.state.as_deref(), Some("SUCCESS"))
        };
        if self.status_check_rollup.iter().any(bad) {
            Some(("\u{274c}", idx::RED))
        } else if self.status_check_rollup.iter().all(good) {
            Some(("\u{2705}", idx::GRN))
        } else {
            Some(("\u{23f3}", idx::YEL))
        }
    }

    /// `feat/a -> main  +10 -2` -- the branch pair and the diff size, which the
    /// fzf row carried and which the mock's column list leaves implicit.
    fn shape(&self) -> String {
        let mut s = String::new();
        if let (Some(h), Some(b)) = (&self.head_ref_name, &self.base_ref_name) {
            s.push_str(&format!("{h} \u{2192} {b}"));
        }
        if self.additions != 0 || self.deletions != 0 {
            if !s.is_empty() {
                s.push_str("  ");
            }
            s.push_str(&format!("+{} \u{2212}{}", self.additions, self.deletions));
        }
        s
    }

    fn age(&self) -> String {
        self.created_at
            .as_deref()
            .and_then(time::epoch)
            .map(|t| time::age((time::now() - t).max(0)))
            .unwrap_or_default()
    }

    fn touched(&self) -> i64 {
        self.updated_at
            .as_deref()
            .and_then(time::epoch)
            .or_else(|| self.created_at.as_deref().and_then(time::epoch))
            .unwrap_or(0)
    }

    fn involves(&self, who: &str) -> bool {
        self.author_login() == who || self.assignees.iter().any(|a| a.login == who)
    }
}

/// One control in the sidebar.
enum Control {
    /// Exactly one of these. Picking collapses, because the question is
    /// answered.
    Pick {
        name: &'static str,
        options: Vec<String>,
        chosen: usize,
    },
    /// Any number. Picking leaves it open, because you are usually choosing
    /// more than one -- which is the interaction the design doc specifies.
    Many {
        name: &'static str,
        options: Vec<String>,
        on: Vec<bool>,
    },
    /// Not a filter: a thing to do.
    Action { name: &'static str, detail: String },
}

impl Control {
    fn name(&self) -> &str {
        match self {
            Control::Pick { name, .. }
            | Control::Many { name, .. }
            | Control::Action { name, .. } => name,
        }
    }

    fn len(&self) -> usize {
        match self {
            Control::Pick { options, .. } | Control::Many { options, .. } => options.len(),
            Control::Action { .. } => 0,
        }
    }

    /// What the collapsed row says it is set to.
    fn summary(&self) -> String {
        match self {
            Control::Pick {
                options, chosen, ..
            } => options.get(*chosen).cloned().unwrap_or_default(),
            Control::Many { options, on, .. } => {
                let n = on.iter().filter(|x| **x).count();
                match n {
                    0 => "any".into(),
                    1 => options
                        .iter()
                        .zip(on)
                        .find(|(_, x)| **x)
                        .map(|(o, _)| o.clone())
                        .unwrap_or_default(),
                    // The count rather than the names: five logins do not fit
                    // and the pane is not here to hold them.
                    n => format!("{n} of {}", options.len()),
                }
            }
            Control::Action { detail, .. } => detail.clone(),
        }
    }

    fn picked(&self) -> Vec<&str> {
        match self {
            Control::Many { options, on, .. } => options
                .iter()
                .zip(on)
                .filter(|(_, x)| **x)
                .map(|(o, _)| o.as_str())
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// Which sidebar control, and which option within it.
enum SideAt {
    Head(usize),
    Option(usize, usize),
    Nothing,
}

/// Which main row.
enum At {
    Bulk,
    Pr(usize),
    Nothing,
}

/// The order of the controls, by index, so the code reads as the design does.
const STATE: usize = 0;
const SORT: usize = 1;
const CREATED: usize = 2;
const ASSIGNED: usize = 3;
const INVOLVES: usize = 4;
const LABELS: usize = 5;

pub struct Prs {
    repo: String,
    /// The clone on this disk, for the commands that work on one.
    dir: PathBuf,
    fetch: Arc<Fetcher>,
    me: String,
    prs: Vec<Pr>,
    /// Indices into `prs`, after the sidebar's filters and the sort.
    shown: Vec<usize>,
    /// Ticked pull request NUMBERS, so a refresh cannot silently move the
    /// selection onto different rows.
    ///
    /// Only numbers still in `prs`: `sync` drops the rest. A filter can hide
    /// a ticked one without unticking it, so what a bulk action touches is
    /// `picked()`, never this.
    ticked: BTreeSet<u64>,
    controls: Vec<Control>,
    open: Vec<bool>,
    epoch_seen: u64,
    error: Option<String>,
    /// `r` was pressed and nothing has landed since. Said in the footer,
    /// because the list on show is still the old one until it does.
    refreshing: bool,
    sel: usize,
    /// `gh pr create` was handed the terminal, so the next tick re-asks.
    stale: bool,
    /// Counted up by a menu each time it runs something, and the count the
    /// list was last asked for at. A menu that was only looked at leaves the
    /// two equal, and nothing is asked again for it.
    writes: Writes,
    wrote_seen: u64,
    /// The sidebar row to put the cursor on after a dropdown closes from one
    /// of its own options. Set for the one `Flow::Retarget` that needs it.
    side_land: Option<usize>,
}

impl Prs {
    pub fn open(repo: &str, me: &str, dir: &Path, fetch: Arc<Fetcher>) -> Self {
        let controls = vec![
            Control::Pick {
                name: "state",
                options: vec![
                    "open".into(),
                    "closed".into(),
                    "merged".into(),
                    "all".into(),
                ],
                chosen: 0,
            },
            Control::Pick {
                name: "sort",
                options: vec!["newest".into(), "oldest".into(), "recently updated".into()],
                chosen: 0,
            },
            Control::Many {
                name: "created by",
                options: Vec::new(),
                on: Vec::new(),
            },
            Control::Many {
                name: "assigned to",
                options: Vec::new(),
                on: Vec::new(),
            },
            Control::Many {
                name: "involves",
                options: Vec::new(),
                on: Vec::new(),
            },
            Control::Many {
                name: "labels",
                options: Vec::new(),
                on: Vec::new(),
            },
            Control::Action {
                name: "new pull request",
                detail: "gh pr create".into(),
            },
        ];
        let open = vec![false; controls.len()];
        let me_ = Self {
            repo: repo.to_string(),
            dir: dir.to_path_buf(),
            fetch,
            me: me.to_string(),
            prs: Vec::new(),
            shown: Vec::new(),
            ticked: BTreeSet::new(),
            controls,
            open,
            epoch_seen: 0,
            error: None,
            refreshing: false,
            sel: 0,
            stale: false,
            writes: Writes::default(),
            wrote_seen: 0,
            side_land: None,
        };
        me_.ask();
        me_
    }

    fn state(&self) -> &str {
        match &self.controls[STATE] {
            Control::Pick {
                options, chosen, ..
            } => options[*chosen].as_str(),
            _ => "open",
        }
    }

    fn req(&self) -> Req {
        self.req_for(self.state())
    }

    /// The list call for one value of the `state` control.
    fn req_for(&self, state: &str) -> Req {
        Req::Cmd {
            args: vec![
                "pr".into(),
                "list".into(),
                "--repo".into(),
                self.repo.clone(),
                "--state".into(),
                state.into(),
                "--limit".into(),
                "50".into(),
                "--json".into(),
                FIELDS.into(),
            ],
        }
    }

    fn ask(&self) {
        self.fetch.request(self.req());
    }

    fn sync(&mut self) {
        self.refreshing = false;
        match self.fetch.get(&self.req()) {
            Some(Got::Json(v)) => match Vec::<Pr>::deserialize(v.as_ref()) {
                Ok(prs) => {
                    self.prs = prs;
                    self.error = None;
                    // A tick is for a pull request that is listed. One that
                    // was merged or closed since is no longer here to be
                    // seen, and a tick nobody can see would still have been
                    // sent to the next bulk action.
                    let prs = &self.prs;
                    self.ticked.retain(|n| prs.iter().any(|p| p.number == *n));
                    self.learn_options();
                }
                Err(e) => self.error = Some(format!("unreadable pull request list: {e}")),
            },
            Some(Got::Denied(d)) => self.error = Some(d.fix()),
            Some(Got::Failed(e)) => self.error = Some(e),
            _ => {}
        }
        self.apply();
    }

    /// Fill the user and label dropdowns from the pull requests themselves.
    ///
    /// `me` is pinned first whether or not they appear, which is the design
    /// doc's "me at the top" -- and it is the option you reach for most, so it
    /// should not move about as the list changes.
    fn learn_options(&mut self) {
        // Unless nobody could say who "me" is, which must not become a blank
        // option at the top of every list.
        let mut users: Vec<String> = Vec::new();
        if !self.me.is_empty() {
            users.push(self.me.clone());
        }
        let mut labels: Vec<String> = Vec::new();
        for pr in &self.prs {
            for who in std::iter::once(pr.author_login().to_string())
                .chain(pr.assignees.iter().map(|a| a.login.clone()))
            {
                if !who.is_empty() && who != "?" && !users.contains(&who) {
                    users.push(who);
                }
            }
            for l in &pr.labels {
                if !labels.contains(&l.name) {
                    labels.push(l.name.clone());
                }
            }
        }
        labels.sort();
        for (i, opts) in [
            (CREATED, users.clone()),
            (ASSIGNED, users.clone()),
            (INVOLVES, users),
            (LABELS, labels),
        ] {
            // Selections are kept by NAME across a refresh: the option list
            // can grow, and re-indexing a Vec<bool> would silently move a
            // tick onto someone else.
            let was: Vec<String> = self.controls[i]
                .picked()
                .into_iter()
                .map(String::from)
                .collect();
            if let Control::Many { options, on, .. } = &mut self.controls[i] {
                *on = opts.iter().map(|o| was.contains(o)).collect();
                *options = opts;
            }
        }
    }

    /// Re-derive `shown` from the controls and the sort.
    fn apply(&mut self) {
        let created = self.controls[CREATED].picked();
        let assigned = self.controls[ASSIGNED].picked();
        let involves = self.controls[INVOLVES].picked();
        let labels = self.controls[LABELS].picked();
        let mut keep: Vec<usize> = (0..self.prs.len())
            .filter(|&i| {
                let pr = &self.prs[i];
                let ok_created = created.is_empty() || created.contains(&pr.author_login());
                let ok_assigned = assigned.is_empty()
                    || pr
                        .assignees
                        .iter()
                        .any(|a| assigned.contains(&a.login.as_str()));
                let ok_involves = involves.is_empty() || involves.iter().any(|w| pr.involves(w));
                let ok_labels = labels.is_empty()
                    || pr.labels.iter().any(|l| labels.contains(&l.name.as_str()));
                ok_created && ok_assigned && ok_involves && ok_labels
            })
            .collect();
        let sort = match &self.controls[SORT] {
            Control::Pick { chosen, .. } => *chosen,
            _ => 0,
        };
        match sort {
            1 => keep.sort_by_key(|&i| self.prs[i].number),
            2 => keep.sort_by_key(|&i| std::cmp::Reverse(self.prs[i].touched())),
            // Newest first, which is what `gh pr list` already gives -- sorted
            // explicitly so the order does not depend on that staying true.
            _ => keep.sort_by_key(|&i| std::cmp::Reverse(self.prs[i].number)),
        }
        self.shown = keep;
    }

    /// The sidebar row a control's own heading is on.
    fn head_row(&self, control: usize) -> usize {
        (0..control)
            .map(|c| {
                1 + if self.open[c] {
                    self.controls[c].len()
                } else {
                    0
                }
            })
            .sum()
    }

    /// Close a dropdown from one of its options, and take the cursor to its
    /// heading. Every row below a closing dropdown moves up, so a cursor left
    /// at the same number was on some other control.
    fn close_from_inside(&mut self, control: usize) -> Flow {
        self.open[control] = false;
        self.side_land = Some(self.head_row(control));
        Flow::Retarget
    }

    fn side_at(&self, sel: usize) -> SideAt {
        let mut row = 0;
        for (c, ctl) in self.controls.iter().enumerate() {
            if sel == row {
                return SideAt::Head(c);
            }
            row += 1;
            if self.open[c] {
                for o in 0..ctl.len() {
                    if sel == row {
                        return SideAt::Option(c, o);
                    }
                    row += 1;
                }
            }
        }
        SideAt::Nothing
    }

    /// True when the pull requests themselves are drawn, under the row that
    /// counts the ticks -- rather than the one row that says why not, when
    /// the list failed, is empty, or is filtered away.
    fn listed(&self) -> bool {
        self.error.is_none() && !self.shown.is_empty()
    }

    /// The ticked pull requests a bulk action would touch: the ones on show,
    /// in the order they are shown.
    ///
    /// Not `ticked` itself. A sidebar filter hides a ticked pull request
    /// without unticking it, and "close all" must mean all of what the row
    /// above the list counted, not that plus some nobody can see.
    fn picked(&self) -> Vec<u64> {
        self.shown
            .iter()
            .map(|&i| self.prs[i].number)
            .filter(|n| self.ticked.contains(n))
            .collect()
    }

    fn at(&self, sel: usize) -> At {
        // A row standing in for the list is a row, and it is not the pull
        // request that would have been first. After a refresh failed the old
        // list is still held here, and without this `enter` on "could not
        // list pull requests" opened the menu of one that was not drawn.
        if !self.listed() {
            return At::Nothing;
        }
        match sel {
            // The counting row is always drawn, and is something to act on
            // only once it counts something.
            0 if self.picked().is_empty() => At::Nothing,
            0 => At::Bulk,
            n => self
                .shown
                .get(n - 1)
                .map(|&i| At::Pr(i))
                .unwrap_or(At::Nothing),
        }
    }

    /// The last tick went while the cursor was on the row that counts them,
    /// which is now a line of text and nowhere for a cursor to be. Send it to
    /// the first pull request.
    fn off_the_count(&mut self) -> Flow {
        self.sel = 1;
        Flow::Retarget
    }

    /// The sidebar's "new pull request", which is run from inside the clone.
    ///
    /// `gh pr create` opens one for the branch that is checked out, and
    /// `--repo` does not change where it looks for that: it reads whatever
    /// git repository the process is standing in, and this is started from
    /// anywhere. In the clone, "the current branch" is the one this screen is
    /// about.
    fn create(&self) -> Flow {
        Flow::DetachIn {
            dir: self.dir.clone(),
            argv: vec![
                "gh".into(),
                "pr".into(),
                "create".into(),
                "--repo".into(),
                self.repo.clone(),
            ],
        }
    }

    fn pr_menu(&self, pr: &Pr) -> Menu {
        let n = pr.number.to_string();
        let repo = self.repo.clone();
        let gh = |args: &[&str]| {
            let mut v = vec!["gh".to_string(), "pr".into()];
            v.extend(args.iter().map(|s| s.to_string()));
            v.extend(["--repo".to_string(), repo.clone()]);
            v
        };
        let dir = self.dir.to_string_lossy().into_owned();
        let mut items = vec![
            // From the clone, because `gh pr checkout` fetches into and
            // switches the repository it is run in, and that was wherever
            // this was started. And it asks, because it moves a working tree.
            Item::new(
                "Checkout locally",
                "gh pr checkout",
                Act::Run(gh(&["checkout", &n])),
            )
            .in_dir(&self.dir)
            .confirm()
            .note(&["Fetches its head and switches the clone at", &dir, "to it."]),
            Item::new("Diff", "gh pr diff", Act::Run(gh(&["diff", &n]))),
            Item::new(
                "Changed files",
                "gh pr diff --name-only",
                Act::Run(gh(&["diff", &n, "--name-only"])),
            ),
            // gh opens an editor or prompts for the body itself.
            Item::new("Comment", "gh pr comment", Act::Run(gh(&["comment", &n]))),
        ];
        // The bash screen's ctrl-a, as a row -- a key nobody is told about is
        // a key nobody finds. It sits with the items above it because those
        // are the ones that only LOOK at the pull request; everything below
        // changes it.
        //
        // Building the screen here fires its run list now rather than when
        // the row is chosen -- the same prefetch the reviewer picker does from
        // the environment menu, and the reason the screen is already drawn
        // when it opens. One request per menu, deduplicated by the fetcher.
        if let Some(sha) = &pr.head_ref_oid {
            // The whole sha goes in the NOTE, because the detail column is
            // narrow enough to truncate it and `Act::Open`'s pane line is just
            // "opens a screen" -- so without this the one fact that
            // distinguishes this item is readable nowhere.
            let full = format!("head {sha}");
            let on = match &pr.head_ref_name {
                Some(b) => format!("Its runs, not {b}'s as it is now."),
                None => "Its runs, not the head branch's as it is now.".into(),
            };
            items.push(
                Item::new(
                    "Workflow runs for this commit",
                    format!("head {}", crate::runs::short(sha)),
                    Act::open({
                        let (repo, sha) = (self.repo.clone(), sha.clone());
                        let (branch, fetch) = (pr.head_ref_name.clone(), Arc::clone(&self.fetch));
                        move || {
                            Box::new(crate::runs::Actions::for_commit(
                                &repo,
                                &sha,
                                branch.as_deref(),
                                Arc::clone(&fetch),
                            ))
                        }
                    }),
                )
                .note(&[&full, &on]),
            );
        }
        if pr.state.as_deref() == Some("OPEN") {
            items.push(
                Item::new(
                    "Approve",
                    "gh pr review --approve",
                    Act::Run(gh(&["review", &n, "--approve"])),
                )
                .confirm()
                .note(&["Posts an approving review as you."]),
            );
            // A field, because gh will not take this one without a reason:
            // with no `--body` it answers "body cannot be blank" and posts
            // nothing. One line is what the field holds, and a review that
            // needs more than a line is better written in the browser.
            items.push(
                Item::new(
                    "Request changes",
                    "gh pr review --request-changes",
                    Act::Prompt {
                        label: format!("what has to change in #{n}"),
                        initial: String::new(),
                        argv: gh(&["review", &n, "--request-changes", "--body", "{}"]),
                    },
                )
                .confirm()
                .note(&["Posts a review as you, asking for changes."]),
            );
            // gh edits the pull request the moment it has the name, and says
            // so only afterwards -- so these ask, after the field.
            items.push(
                Item::new(
                    "Add a label",
                    "gh pr edit --add-label",
                    Act::Prompt {
                        label: format!("label to add to #{n}"),
                        initial: String::new(),
                        argv: gh(&["edit", &n, "--add-label", "{}"]),
                    },
                )
                .confirm(),
            );
            items.push(
                Item::new(
                    "Add a reviewer",
                    "gh pr edit --add-reviewer",
                    Act::Prompt {
                        label: format!("reviewer to add to #{n}"),
                        initial: String::new(),
                        argv: gh(&["edit", &n, "--add-reviewer", "{}"]),
                    },
                )
                .confirm()
                .note(&["Asks them for a review, which notifies them."]),
            );
            // Interactive: gh asks what to change.
            items.push(Item::new(
                "Edit title / body",
                "gh pr edit",
                Act::Run(gh(&["edit", &n])),
            ));
            items.push(
                Item::new("Merge", "gh pr merge", Act::Run(gh(&["merge", &n])))
                    .confirm()
                    .note(&[
                        "gh asks which merge method and whether to delete",
                        "the branch, so this confirms the merge itself.",
                    ]),
            );
            items.push(
                Item::new(
                    if pr.is_draft {
                        "Mark ready"
                    } else {
                        "Mark draft"
                    },
                    if pr.is_draft {
                        "gh pr ready"
                    } else {
                        "gh pr ready --undo"
                    },
                    Act::Run(if pr.is_draft {
                        gh(&["ready", &n])
                    } else {
                        gh(&["ready", &n, "--undo"])
                    }),
                )
                .confirm(),
            );
            items.push(
                Item::new("Close", "gh pr close", Act::Run(gh(&["close", &n])))
                    .danger()
                    .note(&["Closes it without merging. It can be reopened."]),
            );
        }
        items.push(Item::new(
            "Open in browser",
            "gh pr view --web",
            Act::Run(gh(&["view", &n, "--web"])),
        ));
        Menu::new("pull request", format!("#{n}  {}", pr.title), items).marks(&self.writes)
    }

    /// The menu for everything ticked and on show.
    fn bulk_menu(&self) -> Menu {
        let picked = self.picked();
        let count = picked.len();
        let nums = picked
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let repo = self.repo.as_str();
        // One command per item, and gh takes one pull request per command, so
        // each item is the loop in `EACH` around the gh call. The numbers are
        // one argument and the call is the rest, with the number last.
        let each = |cmd: &[&str]| {
            let mut v = vec!["sh".to_string(), "-c".into(), EACH.into(), nums.clone()];
            v.extend(cmd.iter().map(|s| s.to_string()));
            v
        };
        let once = [
            "Runs gh once for each ticked pull request,",
            "because it takes one number at a time.",
        ];
        let mut items = Vec::new();
        for (label, verb) in [("Close all", "close"), ("Reopen all", "reopen")] {
            items.push(
                Item::new(
                    label,
                    format!("gh pr {verb} \u{d7}{count}"),
                    Act::Run(each(&["gh", "pr", verb, "--repo", repo])),
                )
                .danger()
                .note(&once),
            );
        }
        for (label, what, flag) in [
            ("Add a label to all", "label", "--add-label"),
            ("Add a reviewer to all", "reviewer", "--add-reviewer"),
        ] {
            items.push(
                Item::new(
                    label,
                    format!("gh pr edit {flag} \u{d7}{count}"),
                    Act::Prompt {
                        label: format!("{what} for {count} pull requests"),
                        initial: String::new(),
                        argv: each(&["gh", "pr", "edit", "--repo", repo, flag, "{}"]),
                    },
                )
                .confirm()
                .note(&once),
            );
        }
        // No "untick everything" here. A menu row can run a command or close
        // the menu, and neither reaches the set this screen holds -- the row
        // that was here closed the menu and left every tick in place. `space`
        // on the counting row does it, where the set is.
        Menu::new("selection", format!("{count} pull requests"), items).marks(&self.writes)
    }
}

impl Screen for Prs {
    fn title(&self) -> String {
        format!("{} \u{b7} pull requests", self.repo)
    }

    /// Filters left, list right -- the mock's layout, and the reason
    /// `Split::Sidebar` exists.
    fn split(&self) -> Split {
        Split::Sidebar
    }

    fn focus(&mut self, sel: usize) {
        self.sel = sel;
    }

    /// Where the cursor belongs after `Flow::Retarget`: wherever it was last
    /// put, by the app or by this screen. Row 0 until then, which the app
    /// reads as the first row a cursor can rest on.
    fn initial_sel(&self) -> usize {
        self.sel
    }

    fn tick(&mut self) -> Tick {
        // Only the top screen ticks, so what a menu ran is first noticed
        // here once that menu has closed.
        let wrote = self.writes.since(&mut self.wrote_seen);
        if wrote || self.stale {
            self.stale = false;
            self.fetch.refresh(self.req());
            return Tick::Changed;
        }
        if self.fetch.landed(&mut self.epoch_seen) {
            self.sync();
            return Tick::Changed;
        }
        self.fetch.waiting()
    }

    fn sidebar(&mut self) -> Vec<Row> {
        let mut rows = Vec::new();
        for (c, ctl) in self.controls.iter().enumerate() {
            let mut head = Row::leaf(ctl.name());
            if ctl.len() > 0 {
                head.group = true;
                head.expanded = self.open[c];
            }
            rows.push(head.detail(ctl.summary()));
            if !self.open[c] {
                continue;
            }
            match ctl {
                Control::Pick {
                    options, chosen, ..
                } => {
                    for (o, opt) in options.iter().enumerate() {
                        let mut r = Row::leaf(opt);
                        r.depth = 1;
                        if o == *chosen {
                            r = r.badged("\u{25cf}", idx::GRN);
                        }
                        rows.push(r);
                    }
                }
                Control::Many { options, on, .. } => {
                    for (o, opt) in options.iter().enumerate() {
                        let mut r = Row::leaf(opt);
                        r.depth = 1;
                        if on[o] {
                            r = r.badged("\u{2713}", idx::GRN);
                        }
                        rows.push(r);
                    }
                }
                Control::Action { .. } => {}
            }
        }
        rows
    }

    fn rows(&mut self) -> Vec<Row> {
        // Row 0 belongs to the count of ticks in EVERY shape, blank when
        // there is no list to count. So the row a cursor waits on while the
        // list is on its way is the row the first pull request lands on --
        // rather than the count's, which with nothing ticked is not a row a
        // cursor can be on at all.
        let instead = |row: Row| vec![Row::spacer(), row];
        if let Some(e) = &self.error {
            return instead(Row::leaf("could not list pull requests").detail(e.clone()));
        }
        if self.prs.is_empty() {
            return instead(Row::leaf(if self.fetch.pending() {
                "asking github\u{2026}"
            } else {
                "nothing in this state"
            }));
        }
        if self.shown.is_empty() {
            return instead(Row::leaf("no pull request matches these filters"));
        }
        let mut rows = Vec::new();
        // The counting row, drawn whether or not anything is ticked so that
        // ticking never moves a pull request to a different row. With nothing
        // ticked it is a line of text the cursor passes over.
        //
        // A `group` in both states, and never open, because that is the one
        // kind of row `/` always keeps: one that came and went with what was
        // typed would shift the rows under the cursor all over again.
        let picked = self.picked().len();
        let hidden = self.ticked.len() - picked;
        let mut head = Row::leaf(format!(
            "{} {picked} of {} ticked",
            if picked > 0 { "\u{25a3}" } else { "\u{25a1}" },
            self.shown.len()
        ));
        head.group = true;
        head.selectable = picked > 0;
        let mut says = if picked > 0 {
            "enter for what to do with them".to_string()
        } else {
            "space ticks one \u{b7} a ticks all".to_string()
        };
        // A filter can hide a ticked pull request. It stays ticked and no
        // bulk action touches it, and both of those are worth one clause.
        if hidden > 0 {
            says.push_str(&format!(" \u{b7} {hidden} more hidden by the filters"));
        }
        rows.push(head.detail(says));
        for &i in &self.shown {
            let pr = &self.prs[i];
            let (review, rc) = pr.review();
            let box_ = if self.ticked.contains(&pr.number) {
                "\u{25a3}"
            } else {
                "\u{25a1}"
            };
            let mut r = Row::leaf(format!(
                "{box_} {DIM}#{}{OFF} {}",
                pr.number,
                pr.title.replace(['\t', '\n'], " ")
            ))
            .detail(format!(
                "{} \u{b7} {} \u{b7} {}",
                pr.author_login(),
                pr.age(),
                pr.shape()
            ));
            r = r.badged(review, rc);
            if let Some((c, cc)) = pr.checks() {
                r = r.badged(c, cc);
            }
            rows.push(r);
        }
        rows
    }

    /// Never drawn: `Split::Sidebar` has no pane, by the mock's design.
    fn pane(&mut self, _sel: usize, _cols: u16, _rows: u16) -> Pane {
        Pane::default()
    }

    fn initial_side_sel(&self) -> Option<usize> {
        self.side_land
    }

    fn on_sidebar(&mut self, action: Action, sel: usize) -> Flow {
        // Wherever the last closing dropdown sent the cursor, it has been.
        self.side_land = None;
        match (action, self.side_at(sel)) {
            // `l` opens a closed control, and CROSSES to the list once there
            // is nothing left to open -- the edge rule, applied to a column
            // where every row is a dropdown. Without the second half `l` just
            // toggled dropdowns forever and the list was unreachable from
            // here, which is the opposite of navigating freely.
            //
            // Opening is ALL `l` does. It shared an arm with `enter`, so on
            // the action row it ran `gh pr create` -- a key for moving right
            // that started a pull request.
            (Action::Open, SideAt::Head(c)) if !self.open[c] && self.controls[c].len() > 0 => {
                self.open[c] = true;
                Flow::Dirty
            }
            (Action::Open, _) => Flow::Continue,
            (Action::Activate, SideAt::Head(c)) => {
                // The action row. `gh pr create` prompts for the title and
                // body itself, which is why this needs no field. The list is
                // re-asked afterwards, since there may be one more in it.
                if let Control::Action { .. } = self.controls[c] {
                    self.stale = true;
                    return self.create();
                }
                // A dropdown with nothing in it has nothing to open.
                if self.controls[c].len() == 0 {
                    return Flow::Continue;
                }
                self.open[c] = !self.open[c];
                Flow::Dirty
            }
            (Action::Close, SideAt::Head(c)) => {
                self.open[c] = false;
                Flow::Dirty
            }
            // `h` on an option closes the control it belongs to, leaving the
            // cursor on its row. The design doc describes left-then-enter to
            // get the same place; one key is the same end state and matches
            // what `h` already means on a step row.
            (Action::Close, SideAt::Option(c, _)) => self.close_from_inside(c),
            (Action::Activate, SideAt::Option(c, o)) => {
                let refetch = c == STATE;
                let mut flow = Flow::Dirty;
                match &mut self.controls[c] {
                    Control::Pick { chosen, .. } => {
                        *chosen = o;
                        // Answered, so it closes.
                        flow = self.close_from_inside(c);
                    }
                    Control::Many { on, .. } => on[o] = !on[o],
                    Control::Action { .. } => {}
                }
                if refetch {
                    // Only `state` changes which pull requests EXIST.
                    self.prs.clear();
                    self.ticked.clear();
                    // The error was about the state being left. Kept, it
                    // drew "could not list" over a list still on its way.
                    self.error = None;
                    self.ask();
                    // And read what is already there. `ask` does nothing for
                    // a state fetched a moment ago, so nothing arrives to
                    // wake `tick`, and going back to `open` sat on "nothing
                    // in this state" with the answer in hand.
                    self.sync();
                } else {
                    self.apply();
                }
                flow
            }
            _ => Flow::Continue,
        }
    }

    fn on_action(&mut self, action: Action, sel: usize) -> Flow {
        match (action, self.at(sel)) {
            // Space ticks, the same key that toggles a section in `dots` --
            // and deliberately not enter, which opens.
            (Action::Key(' '), At::Pr(i)) => {
                let n = self.prs[i].number;
                if !self.ticked.remove(&n) {
                    self.ticked.insert(n);
                }
                Flow::Dirty
            }
            // On the counting row the box is the whole set, so `space`
            // empties it -- hidden ticks included, or "none ticked" would
            // leave some armed behind a filter.
            (Action::Key(' '), At::Bulk) => {
                self.ticked.clear();
                self.off_the_count()
            }
            // Everything on show, or nothing at all once everything on show
            // already is. Compared as SETS: a filter can leave as many ticks
            // hidden as there are rows showing, and counting them called that
            // "all ticked" and cleared instead.
            (Action::Key('a'), _) if self.listed() => {
                let shown: BTreeSet<u64> = self.shown.iter().map(|&i| self.prs[i].number).collect();
                if shown.is_subset(&self.ticked) {
                    self.ticked.clear();
                } else {
                    self.ticked.extend(shown);
                }
                if sel == 0 && self.picked().is_empty() {
                    return self.off_the_count();
                }
                Flow::Dirty
            }
            // Both count what they run, and `tick` re-asks for it.
            (Action::Activate, At::Bulk) => Flow::Push(Box::new(self.bulk_menu())),
            (Action::Activate, At::Pr(i)) => Flow::Push(Box::new(self.pr_menu(&self.prs[i]))),
            (Action::Key('o'), At::Pr(i)) => {
                let n = self.prs[i].number.to_string();
                Flow::Detach(vec![
                    "gh".into(),
                    "pr".into(),
                    "view".into(),
                    n,
                    "--repo".into(),
                    self.repo.clone(),
                    "--web".into(),
                ])
            }
            (Action::Key('r'), _) => {
                self.fetch.refresh(self.req());
                self.refreshing = true;
                Flow::Dirty
            }
            _ => Flow::Continue,
        }
    }

    fn footer(&self) -> String {
        let legend = format!("{DIM}space tick \u{b7} a tick all \u{b7} h for the filters{OFF}");
        let r = if self.refreshing {
            "refreshing\u{2026}"
        } else {
            "r refresh"
        };
        match self.at(self.sel) {
            At::Bulk => format!(
                "enter what to do with the ticked ones \u{b7} space unticks them \u{b7} {r}\n{legend}"
            ),
            At::Pr(_) => {
                format!("enter what you can do to it \u{b7} o browser \u{b7} {r}\n{legend}")
            }
            At::Nothing => format!("{r}\n{legend}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menu::check;

    const RAW: &str = r#"[
      {"number":70,"title":"Add the thing","author":{"login":"roest1"},"assignees":[],
       "headRefName":"feat/a","headRefOid":"1a2b3c4d5e6f7a8b9c0d","baseRefName":"main","reviewDecision":"REVIEW_REQUIRED",
       "isDraft":false,"createdAt":"2026-10-22T10:00:00Z","updatedAt":"2026-10-23T10:00:00Z",
       "additions":10,"deletions":2,"labels":[{"name":"enhancement"}],
       "statusCheckRollup":[{"conclusion":"SUCCESS","state":null,"status":"COMPLETED"}],
       "state":"OPEN"},
      {"number":69,"title":"Fix the other","author":{"login":"someone"},
       "assignees":[{"login":"roest1"}],"headRefName":"fix/b","headRefOid":"aaaabbbbccccddddeeee","baseRefName":"main",
       "reviewDecision":"APPROVED","isDraft":false,"createdAt":"2026-10-20T10:00:00Z",
       "updatedAt":"2026-10-24T10:00:00Z","additions":1,"deletions":1,
       "labels":[{"name":"bug"}],
       "statusCheckRollup":[{"conclusion":"FAILURE","state":null,"status":"COMPLETED"}],
       "state":"OPEN"},
      {"number":68,"title":"Draft idea","author":{"login":"roest1"},"assignees":[],
       "headRefName":"wip","headRefOid":"ffff0000111122223333","baseRefName":"main","reviewDecision":null,"isDraft":true,
       "createdAt":"2026-10-19T10:00:00Z","updatedAt":"2026-10-19T10:00:00Z",
       "additions":0,"deletions":0,"labels":[],"statusCheckRollup":[],"state":"OPEN"}
    ]"#;

    const CLONE: &str = "/clones/dotfiles";

    fn screen() -> Prs {
        let mut p = Prs::open(
            "roest1/dotfiles",
            "roest1",
            Path::new(CLONE),
            Arc::new(Fetcher::new("")),
        );
        p.prs = serde_json::from_str(RAW).expect("parses");
        p.learn_options();
        p.apply();
        p
    }

    /// Open a sidebar dropdown and press enter on one of its options.
    fn pick(p: &mut Prs, control: usize, option: &str) {
        p.open[control] = true;
        let row = p
            .sidebar()
            .iter()
            .position(|r| r.label == option)
            .unwrap_or_else(|| panic!("no option {option}"));
        p.on_sidebar(Action::Activate, row);
    }

    /// Put `raw` in the fetcher as the answer for `state`, the way a list
    /// that came back earlier is.
    ///
    /// Never for `open`: `Prs::open` asks for that one for real, and whatever
    /// the worker comes back with would land on top of what was put here.
    fn seed(p: &Prs, state: &str, raw: &str) {
        let v: serde_json::Value = serde_json::from_str(raw).expect("parses");
        p.fetch.seed(p.req_for(state), Got::Json(Arc::new(v)));
    }

    /// The pull request numbers on show, in row order.
    fn nums(p: &Prs) -> Vec<u64> {
        p.shown.iter().map(|&i| p.prs[i].number).collect()
    }

    /// Every command this screen can run: the label of what runs it, whether
    /// that asks first, and the argv.
    fn commands(p: &mut Prs) -> Vec<(String, bool, Vec<String>, Option<PathBuf>)> {
        let mut closed = p.prs[0].clone();
        closed.state = Some("CLOSED".into());
        p.ticked = nums(p).into_iter().collect();
        let menus = [
            p.pr_menu(&p.prs[0]),
            p.pr_menu(&p.prs[2]),
            p.pr_menu(&closed),
            p.bulk_menu(),
        ];
        let mut out = Vec::new();
        for m in &menus {
            for item in m.items() {
                let Some(argv) = check::argv_of(item) else {
                    continue;
                };
                out.push((
                    item.label.clone(),
                    item.asks,
                    argv.clone(),
                    item.dir.clone(),
                ));
            }
        }
        // And the two that are not menu rows.
        match p.create() {
            Flow::DetachIn { dir, argv } => {
                out.push(("new pull request".into(), false, argv, Some(dir)))
            }
            _ => panic!("new pull request runs from inside the clone"),
        }
        match p.on_action(Action::Key('o'), 1) {
            Flow::Detach(argv) => out.push(("o".into(), false, argv, None)),
            _ => panic!("o on a pull request opens it"),
        }
        out
    }

    /// The `gh` call inside a command: itself, or what the bulk loop runs.
    fn inner(argv: &[String]) -> &[String] {
        match argv {
            [sh, c, script, _, rest @ ..] if sh == "sh" && c == "-c" && script == EACH => rest,
            _ => argv,
        }
    }

    /// The whole payload parses, including the camelCase gh gives back and the
    /// nulls it leaves in.
    #[test]
    fn the_gh_pr_list_payload_parses() {
        let p = screen();
        assert_eq!(p.prs.len(), 3);
        assert_eq!(p.prs[0].author_login(), "roest1");
        assert_eq!(p.prs[0].review().0, "\u{1f440} review");
        assert_eq!(p.prs[1].review().0, "\u{2705} approved");
        assert_eq!(p.prs[2].review().0, "\u{1f4dd} draft");
    }

    /// The rollup collapses the way the fzf list did: any failure is a
    /// failure, nothing reported is nothing shown.
    #[test]
    fn the_check_rollup_collapses_to_one_mark() {
        let p = screen();
        assert_eq!(p.prs[0].checks().map(|c| c.0), Some("\u{2705}"));
        assert_eq!(p.prs[1].checks().map(|c| c.0), Some("\u{274c}"));
        assert_eq!(p.prs[2].checks(), None, "no checks is not a green tick");
    }

    /// Green means every check said it passed. A check that was cancelled,
    /// timed out, never started or wants someone to act did not, and one that
    /// reports something this has never heard of is outstanding, not fine.
    #[test]
    fn a_check_that_did_not_pass_is_not_a_green_tick() {
        let mut pr = screen().prs.remove(0);
        let ok = Check {
            conclusion: Some("SUCCESS".into()),
            state: None,
        };
        let run = |conclusion: &str| Check {
            conclusion: Some(conclusion.into()),
            state: None,
        };
        for bad in [
            "FAILURE",
            "CANCELLED",
            "TIMED_OUT",
            "ACTION_REQUIRED",
            "STARTUP_FAILURE",
        ] {
            pr.status_check_rollup = vec![ok.clone(), run(bad)];
            assert_eq!(pr.checks().map(|c| c.0), Some("\u{274c}"), "{bad}");
        }
        // Still running: gh sends an empty conclusion until there is one.
        for open in ["", "STALE", "SOMETHING_NEW"] {
            pr.status_check_rollup = vec![ok.clone(), run(open)];
            assert_eq!(pr.checks().map(|c| c.0), Some("\u{23f3}"), "{open:?}");
        }
        for fine in ["SUCCESS", "NEUTRAL", "SKIPPED"] {
            pr.status_check_rollup = vec![ok.clone(), run(fine)];
            assert_eq!(pr.checks().map(|c| c.0), Some("\u{2705}"), "{fine}");
        }
        // A commit status has a `state` and no conclusion at all.
        let status = |state: &str| Check {
            conclusion: None,
            state: Some(state.into()),
        };
        for (state, mark) in [
            ("SUCCESS", "\u{2705}"),
            ("PENDING", "\u{23f3}"),
            ("EXPECTED", "\u{23f3}"),
            ("ERROR", "\u{274c}"),
            ("FAILURE", "\u{274c}"),
        ] {
            pr.status_check_rollup = vec![ok.clone(), status(state)];
            assert_eq!(pr.checks().map(|c| c.0), Some(mark), "{state}");
        }
    }

    /// `me` is pinned first whether or not they have a pull request, because
    /// it is the option reached for most and must not move about.
    #[test]
    fn the_user_dropdowns_put_me_first() {
        let p = screen();
        for i in [CREATED, ASSIGNED, INVOLVES] {
            match &p.controls[i] {
                Control::Many { options, .. } => assert_eq!(options[0], "roest1"),
                _ => panic!("expected a multi-select"),
            }
        }
        match &p.controls[LABELS] {
            Control::Many { options, .. } => assert_eq!(options, &["bug", "enhancement"]),
            _ => panic!("expected a multi-select"),
        }
    }

    /// Each filter narrows on its own field. `involves` is the one that is not
    /// just an equality test -- it is author OR assignee.
    #[test]
    fn each_filter_narrows_on_its_own_field() {
        let mut p = screen();
        assert_eq!(p.shown.len(), 3);

        // created by someone -> only #69
        if let Control::Many { options, on, .. } = &mut p.controls[CREATED] {
            let at = options
                .iter()
                .position(|o| o == "someone")
                .expect("someone");
            on[at] = true;
        }
        p.apply();
        assert_eq!(
            p.shown.iter().map(|&i| p.prs[i].number).collect::<Vec<_>>(),
            vec![69]
        );

        // and assigned to roest1 is also #69, by a different field
        if let Control::Many { on, .. } = &mut p.controls[CREATED] {
            on.iter_mut().for_each(|x| *x = false);
        }
        if let Control::Many { options, on, .. } = &mut p.controls[ASSIGNED] {
            let at = options.iter().position(|o| o == "roest1").expect("me");
            on[at] = true;
        }
        p.apply();
        assert_eq!(
            p.shown.iter().map(|&i| p.prs[i].number).collect::<Vec<_>>(),
            vec![69]
        );
    }

    /// Involvement is author OR assignee, which is why it is its own control
    /// rather than the other two combined.
    #[test]
    fn involves_catches_both_ends() {
        let mut p = screen();
        if let Control::Many { options, on, .. } = &mut p.controls[INVOLVES] {
            let at = options.iter().position(|o| o == "roest1").expect("me");
            on[at] = true;
        }
        p.apply();
        // #70 and #68 authored by me, #69 assigned to me.
        assert_eq!(p.shown.len(), 3);
    }

    #[test]
    fn the_sort_control_reorders_without_refetching() {
        let mut p = screen();
        let nums = |p: &Prs| p.shown.iter().map(|&i| p.prs[i].number).collect::<Vec<_>>();
        assert_eq!(nums(&p), vec![70, 69, 68], "newest first");
        if let Control::Pick { chosen, .. } = &mut p.controls[SORT] {
            *chosen = 1;
        }
        p.apply();
        assert_eq!(nums(&p), vec![68, 69, 70], "oldest first");
        if let Control::Pick { chosen, .. } = &mut p.controls[SORT] {
            *chosen = 2;
        }
        p.apply();
        assert_eq!(nums(&p), vec![69, 70, 68], "recently updated");
    }

    /// The invariant every tree screen here carries, for BOTH regions.
    #[test]
    fn both_walks_match_their_rows() {
        let mut p = screen();
        p.open = vec![true; p.controls.len()];
        let side = p.sidebar();
        for (i, row) in side.iter().enumerate() {
            match p.side_at(i) {
                SideAt::Head(c) => assert_eq!(row.label, p.controls[c].name()),
                SideAt::Option(c, o) => match &p.controls[c] {
                    Control::Pick { options, .. } | Control::Many { options, .. } => {
                        assert_eq!(row.label, options[o])
                    }
                    Control::Action { .. } => panic!("an action has no options"),
                },
                SideAt::Nothing => panic!("sidebar row {i} ({}) resolves to nothing", row.label),
            }
        }
        // The list in every shape it has. A row that is not a pull request
        // and not the count of ticks must walk to nothing, so no key can act
        // on a pull request that row is standing in front of.
        let agree = |p: &mut Prs, shape: &str, want: usize| {
            let rows = p.rows();
            assert_eq!(rows.len(), want, "{shape}");
            for (i, row) in rows.iter().enumerate() {
                let label = crate::paint::unstyled(&row.label);
                match p.at(i) {
                    At::Pr(x) => assert!(
                        label.contains(&format!("#{} ", p.prs[x].number)),
                        "{shape}: row {i} is {label}"
                    ),
                    At::Bulk => assert!(
                        label.contains("ticked") && row.selectable,
                        "{shape}: row {i} is {label}"
                    ),
                    At::Nothing => assert!(
                        !label.contains('#') && (i > 0 || !row.selectable),
                        "{shape}: row {i} ({label}) walks to nothing"
                    ),
                }
            }
            // Row 1 is where a cursor waits, whatever is drawn there.
            assert!(rows[1].selectable, "{shape}");
            assert!(
                matches!(p.at(rows.len()), At::Nothing),
                "{shape}: past the end"
            );
        };
        agree(&mut p, "nothing ticked", 4);
        p.on_action(Action::Key(' '), 2);
        agree(&mut p, "one ticked", 4);

        // Filtered away: one row saying so, under the count's blank one.
        pick(&mut p, LABELS, "bug");
        pick(&mut p, CREATED, "roest1");
        assert!(p.shown.is_empty());
        agree(&mut p, "nothing matches", 2);
        pick(&mut p, LABELS, "bug");
        pick(&mut p, CREATED, "roest1");
        agree(&mut p, "filters off again", 4);

        // A refresh that failed: the old list is still held, and one row is
        // drawn in place of all of it.
        p.error = Some("HTTP 502".into());
        assert!(!p.prs.is_empty() && !p.ticked.is_empty());
        agree(&mut p, "failed", 2);
        for key in [
            Action::Activate,
            Action::Key(' '),
            Action::Key('o'),
            Action::Key('a'),
        ] {
            for row in 0..3 {
                assert!(
                    matches!(p.on_action(key, row), Flow::Continue),
                    "{key:?} on row {row} acted on a pull request behind the error row"
                );
            }
        }
        assert_eq!(
            p.ticked.len(),
            1,
            "a key changed the ticks behind the error row"
        );
        p.error = None;

        // Nothing in this state, or nothing back yet.
        p.prs.clear();
        p.apply();
        agree(&mut p, "empty", 2);
    }

    /// The first pull request arrives under the cursor. While the list is on
    /// its way the cursor waits on row 1, below a blank row 0, and row 1 is
    /// where the first pull request is drawn once there is a list.
    #[test]
    fn the_list_lands_with_the_cursor_on_a_pull_request() {
        let mut p = screen();
        seed(&p, "merged", RAW);
        p.prs.clear();
        p.apply();
        let waiting = p.rows();
        assert!(!waiting[0].selectable && waiting[0].label.is_empty());
        let at = waiting.iter().position(|r| r.selectable).expect("a row");
        pick(&mut p, STATE, "merged");
        assert!(
            matches!(p.at(at), At::Pr(_)),
            "the cursor was left on the count"
        );
    }

    /// Ticking does not move anything. The row that counts the ticks is there
    /// before the first one and after the last, so the pull request under the
    /// cursor is the same one on the next keypress.
    #[test]
    fn ticking_never_moves_a_pull_request_to_another_row() {
        let mut p = screen();
        let under = |p: &Prs, row: usize| match p.at(row) {
            At::Pr(i) => p.prs[i].number,
            _ => panic!("row {row} is not a pull request"),
        };
        assert!(
            matches!(p.at(0), At::Nothing) && !p.rows()[0].selectable,
            "the count is a place to rest before anything is ticked"
        );
        for row in 1..=3 {
            let n = under(&p, row);
            assert!(matches!(p.on_action(Action::Key(' '), row), Flow::Dirty));
            assert!(p.ticked.contains(&n));
            assert_eq!(under(&p, row), n, "ticking #{n} moved it off row {row}");
            assert_eq!(p.rows().len(), 4);
            assert!(matches!(p.at(0), At::Bulk));
            assert!(p.rows()[0].label.contains("1 of 3"));
            // And the same key, on the same row, takes it back.
            p.on_action(Action::Key(' '), row);
            assert!(
                p.ticked.is_empty(),
                "the second space ticked something else"
            );
            assert_eq!(under(&p, row), n, "unticking #{n} moved it off row {row}");
            assert_eq!(p.rows().len(), 4);
        }
        // `a` likewise, in both directions.
        let n = under(&p, 2);
        p.on_action(Action::Key('a'), 2);
        assert_eq!(under(&p, 2), n);
        p.on_action(Action::Key('a'), 2);
        assert_eq!(under(&p, 2), n);
    }

    /// `space` on the count unticks everything, since a menu row cannot. The
    /// row then stops being one a cursor can rest on, so the cursor is sent
    /// to the first pull request.
    #[test]
    fn space_on_the_count_unticks_everything() {
        let mut p = screen();
        p.on_action(Action::Key('a'), 1);
        p.focus(0);
        assert!(p.footer().contains("space unticks"), "{}", p.footer());
        assert!(matches!(p.on_action(Action::Key(' '), 0), Flow::Retarget));
        assert!(p.ticked.is_empty());
        assert_eq!(p.initial_sel(), 1);
        assert!(matches!(p.at(p.initial_sel()), At::Pr(_)));

        // `a` from the count, when it empties the set, moves the same way.
        p.on_action(Action::Key('a'), 1);
        assert!(matches!(p.on_action(Action::Key('a'), 0), Flow::Retarget));
        assert!(p.ticked.is_empty());
        assert_eq!(p.initial_sel(), 1);
    }

    /// A tick is dropped when its pull request is no longer in the list, so
    /// one merged since cannot be sent to a bulk action nobody ticked it for.
    #[test]
    fn a_tick_does_not_outlive_its_pull_request() {
        let mut p = screen();
        seed(&p, "all", RAW);
        pick(&mut p, STATE, "all");
        assert_eq!(nums(&p), vec![70, 69, 68]);
        p.on_action(Action::Key('a'), 1);
        assert_eq!(p.ticked.len(), 3);

        // The same list comes back without #70.
        let mut fewer: Vec<serde_json::Value> = serde_json::from_str(RAW).expect("parses");
        fewer.remove(0);
        seed(&p, "all", &serde_json::to_string(&fewer).expect("writes"));
        p.sync();
        assert_eq!(nums(&p), vec![69, 68]);
        assert_eq!(
            p.ticked.iter().copied().collect::<Vec<_>>(),
            vec![68, 69],
            "a tick was kept for a pull request that is gone"
        );
    }

    /// What a bulk action touches is what is ticked AND on show. A filter
    /// hides a ticked pull request without unticking it, and the count says
    /// that it has.
    #[test]
    fn a_bulk_action_touches_only_what_is_ticked_and_on_show() {
        let mut p = screen();
        p.on_action(Action::Key('a'), 1);
        pick(&mut p, CREATED, "someone");
        assert_eq!(nums(&p), vec![69]);
        assert_eq!(p.ticked.len(), 3, "a filter unticked something");

        let head = p.rows().remove(0);
        assert!(head.label.contains("1 of 1 ticked"), "{}", head.label);
        let says = head.detail.unwrap_or_default();
        assert!(says.contains("2 more hidden by the filters"), "{says}");

        let m = p.bulk_menu();
        assert_eq!(m.title(), "selection \u{b7} 1 pull requests");
        for item in m.items() {
            let Some(argv) = check::argv_of(item) else {
                continue;
            };
            assert_eq!(argv[3], "69", "{}: {argv:?}", item.label);
            assert!(
                !argv.iter().any(|a| a.contains("70") || a.contains("68")),
                "{} reaches a pull request the filters hid: {argv:?}",
                item.label
            );
        }
    }

    /// `a` asks whether everything on show is ticked, not whether as many
    /// are ticked as are on show -- those differ as soon as a filter hides a
    /// tick.
    #[test]
    fn a_compares_what_is_ticked_with_what_is_shown() {
        let mut p = screen();
        p.on_action(Action::Key(' '), 3); // #68
        pick(&mut p, CREATED, "someone");
        assert_eq!(nums(&p), vec![69]);
        // One ticked, one shown, and they are not the same one.
        p.on_action(Action::Key('a'), 1);
        assert!(p.ticked.contains(&69), "a cleared instead of ticking #69");
        // Now everything on show is ticked, so `a` clears -- all of it,
        // rather than leave a tick armed behind the filter.
        p.on_action(Action::Key('a'), 1);
        assert!(p.ticked.is_empty());
    }

    /// Ticks are kept by NUMBER, not by position, so a refresh that reorders
    /// or shortens the list cannot move a tick onto a different pull request.
    #[test]
    fn ticks_survive_a_reorder_because_they_are_numbers() {
        let mut p = screen();
        p.on_action(Action::Key(' '), 1); // #70
        assert!(p.ticked.contains(&70));
        if let Control::Pick { chosen, .. } = &mut p.controls[SORT] {
            *chosen = 1;
        }
        p.apply();
        assert!(p.ticked.contains(&70), "the tick followed the row index");
        assert_eq!(p.ticked.len(), 1);
    }

    /// And a selection kept by name survives the option list growing, which a
    /// Vec<bool> indexed by position would not.
    #[test]
    fn a_selection_survives_a_new_option_appearing() {
        let mut p = screen();
        if let Control::Many { options, on, .. } = &mut p.controls[CREATED] {
            let at = options
                .iter()
                .position(|o| o == "someone")
                .expect("someone");
            on[at] = true;
        }
        // A fourth pull request from a new author arrives, sorting before.
        let mut more: Vec<Pr> = serde_json::from_str(RAW).expect("parses");
        more[0].author = Some(Who {
            login: "aaa".into(),
        });
        more[0].number = 99;
        p.prs.push(more.remove(0));
        p.learn_options();
        assert_eq!(p.controls[CREATED].picked(), vec!["someone"]);
    }

    /// `a` ticks everything shown and untick-alls when everything already is.
    #[test]
    fn a_ticks_all_of_what_is_shown() {
        let mut p = screen();
        p.on_action(Action::Key('a'), 1);
        assert_eq!(p.ticked.len(), 3);
        p.on_action(Action::Key('a'), 1);
        assert!(p.ticked.is_empty());
    }

    /// A closed pull request cannot be approved or merged, so those items are
    /// not offered -- the same rule the branch menu follows.
    #[test]
    fn the_menu_offers_only_what_applies_to_that_pull_request() {
        let p = screen();
        let labels = |m: &mut Menu| m.rows().iter().map(|r| r.label.clone()).collect::<Vec<_>>();

        let open = labels(&mut p.pr_menu(&p.prs[0]));
        assert!(open.iter().any(|s| s == "Merge"), "{open:?}");
        assert!(open.iter().any(|s| s == "Approve"), "{open:?}");
        assert!(open.iter().any(|s| s == "Mark draft"), "{open:?}");

        let mut closed = p.prs[0].clone();
        closed.state = Some("CLOSED".into());
        let shut = labels(&mut p.pr_menu(&closed));
        assert!(!shut.iter().any(|s| s == "Merge"), "{shut:?}");
        assert!(!shut.iter().any(|s| s == "Approve"), "{shut:?}");
        assert!(shut.iter().any(|s| s == "Open in browser"), "{shut:?}");

        // A draft offers to become ready rather than to become a draft.
        let draft = labels(&mut p.pr_menu(&p.prs[2]));
        assert!(draft.iter().any(|s| s == "Mark ready"), "{draft:?}");
    }

    /// The pull request screen's ctrl-a, as a row: the runs for the head
    /// COMMIT, not for the head branch, because a branch answers for whatever
    /// is on it now rather than for what was reviewed. It is offered whatever
    /// state the pull request is in -- a merged one's runs are exactly what
    /// you go looking for afterwards.
    #[test]
    fn the_menu_reaches_the_runs_for_the_head_commit() {
        let p = screen();
        let mut m = p.pr_menu(&p.prs[0]);
        let row = m
            .rows()
            .into_iter()
            .find(|r| r.label == "Workflow runs for this commit")
            .expect("no way to the runs");
        // Seven characters, the length git and github.com both show.
        assert_eq!(row.detail.as_deref(), Some("head 1a2b3c4"));

        let mut closed = p.prs[0].clone();
        closed.state = Some("MERGED".into());
        assert!(
            p.pr_menu(&closed)
                .rows()
                .iter()
                .any(|r| r.label == "Workflow runs for this commit"),
            "a merged pull request lost the way to its runs"
        );

        // And one that came back without a head sha offers nothing rather
        // than a query for a commit called `None`.
        let mut headless = p.prs[0].clone();
        headless.head_ref_oid = None;
        assert!(
            !p.pr_menu(&headless)
                .rows()
                .iter()
                .any(|r| r.label == "Workflow runs for this commit"),
            "offered runs for a commit it does not know"
        );
    }

    /// The bulk menu names how many it will touch, and every item that changes
    /// something asks first.
    #[test]
    fn the_bulk_menu_says_how_many_and_asks() {
        let mut p = screen();
        p.on_action(Action::Key('a'), 1);
        let mut m = p.bulk_menu();
        let rows = m.rows();
        assert!(rows.iter().any(|r| r.label == "Close all"));
        assert!(
            !rows.iter().any(|r| r.label.contains("Untick")),
            "a row that says it unticks, in a menu that cannot"
        );
        // One row and one confirm for all three, which the pane names.
        match m.on_action(Action::Activate, 0) {
            Flow::Dirty => {}
            _ => panic!("a destructive bulk item must ask first"),
        }
        let pane = m.pane(0, 60, 40).lines.join("\n");
        assert!(
            pane.contains("68") && pane.contains("69") && pane.contains("70"),
            "{pane}"
        );
    }

    /// gh takes ONE pull request per `close`, `reopen` or `edit`, so a bulk
    /// item is a loop around the call. Run here for real, with `echo` standing
    /// in for gh: once per number, the number last, and whatever was typed
    /// into the field arriving as one argument that no shell has read.
    #[test]
    fn a_bulk_item_runs_gh_once_for_each_pull_request() {
        assert!(!EACH.contains("{}"), "the menu would type into the script");
        let mut p = screen();
        p.on_action(Action::Key('a'), 1);
        let typed = "needs work; $(touch /nope) *";
        let m = p.bulk_menu();
        let mut ran = 0;
        for item in m.items() {
            let argv: Vec<String> = match &item.act {
                Act::Run(v) => v.clone(),
                Act::Prompt { argv, .. } => argv.iter().map(|a| a.replace("{}", typed)).collect(),
                _ => continue,
            };
            assert_eq!(&argv[..3], ["sh", "-c", EACH], "{}", item.label);
            assert_eq!(argv[3], "70 69 68", "{}", item.label);
            assert_eq!(&argv[4..6], ["gh", "pr"], "{}", item.label);
            let out = std::process::Command::new("sh")
                .args(&argv[1..4])
                .arg("echo")
                .args(&argv[5..])
                .output()
                .expect("sh runs");
            assert!(out.status.success());
            let said = String::from_utf8_lossy(&out.stdout);
            let lines: Vec<&str> = said.lines().collect();
            let call = argv[5..].join(" ");
            assert_eq!(
                lines,
                ["70", "69", "68"].map(|n| format!("{call} {n}")),
                "{}",
                item.label
            );
            ran += 1;
        }
        assert_eq!(ran, 4, "close, reopen, label, reviewer");

        // One that fails does not stop the rest, and is not forgotten by the
        // time the last one succeeds.
        let status = std::process::Command::new("sh")
            .args(["-c", EACH, "1 2", "test", "2", "-eq"])
            .status()
            .expect("sh runs");
        assert_eq!(status.code(), Some(1));
    }

    /// Convention, not luck: every row that changes a pull request or a
    /// working tree waits for a `y`. The exceptions are the commands that
    /// only read, and the ones where gh stops and asks for itself.
    #[test]
    fn every_item_that_changes_something_asks_first() {
        let mut p = screen();
        let mut seen = Vec::new();
        for (label, asks, argv, _) in commands(&mut p) {
            let cmd = inner(&argv);
            assert_eq!(&cmd[..2], ["gh", "pr"], "{label}: {argv:?}");
            // gh stops for a comment's body, a new pull request's title, and
            // -- with no flag saying what to change -- what to edit.
            assert!(
                check::asks_first(asks, cmd),
                "{label} changes something on one enter: {argv:?}"
            );
            seen.push(label);
        }
        for label in [
            "Checkout locally",
            "Request changes",
            "Add a label",
            "Add a reviewer",
            "Add a label to all",
            "Add a reviewer to all",
            "Close all",
        ] {
            assert!(seen.iter().any(|s| s == label), "{label} was not looked at");
        }
    }

    /// `github` is started from anywhere, so no command may lean on where.
    /// Every one names the repo, and the two that work on a git repository
    /// -- `gh` has no `-C` -- are run from inside the clone.
    #[test]
    fn no_command_depends_on_the_working_directory() {
        let mut p = screen();
        let mut local = 0;
        for (label, _, argv, dir) in commands(&mut p) {
            let cmd = inner(&argv);
            assert_eq!(cmd[0], "gh", "{label}: {argv:?}");
            assert!(
                check::names_repo(cmd, "roest1/dotfiles"),
                "{label} names no repo: {argv:?}"
            );
            if matches!(cmd[2].as_str(), "checkout" | "create") {
                assert_eq!(
                    dir.as_deref(),
                    Some(std::path::Path::new(CLONE)),
                    "{label} runs wherever this was started"
                );
                local += 1;
            } else {
                assert_eq!(dir, None, "{label} has no business in the clone");
            }
        }
        assert_eq!(local, 4, "checkout on three menus, and new pull request");

        // A row that names a directory hands it over WITH the command, once
        // the `y` it waits for arrives.
        let mut m = p.pr_menu(&p.prs[0]);
        m.on_action(Action::Activate, 0);
        match m.on_action(Action::Key('y'), 0) {
            Flow::DetachIn { dir, argv } => {
                assert_eq!(dir, std::path::Path::new(CLONE));
                assert_eq!(&argv[..3], ["gh", "pr", "checkout"]);
            }
            _ => panic!("checkout runs from inside the clone"),
        }
        // And the sidebar's enter is the command `create` builds.
        let action = p.controls.len() - 1;
        assert!(matches!(
            p.on_sidebar(Action::Activate, action),
            Flow::DetachIn { .. }
        ));
        assert!(p.stale, "a new pull request was not followed by a re-read");
    }

    /// gh refuses a request for changes with no body, so the row asks for
    /// one, then confirms the command with it filled in.
    #[test]
    fn requesting_changes_asks_what_has_to_change() {
        let p = screen();
        let mut m = p.pr_menu(&p.prs[0]);
        let row = m
            .rows()
            .iter()
            .position(|r| r.label == "Request changes")
            .expect("offered on an open pull request");
        assert!(matches!(
            m.on_action(Action::Activate, row),
            Flow::Ask { .. }
        ));
        assert!(matches!(
            m.answered(Some("needs a test".into())),
            Flow::Dirty
        ));
        match m.on_action(Action::Key('y'), row) {
            Flow::Detach(argv) => assert!(
                argv.windows(3)
                    .any(|w| w == ["--request-changes", "--body", "needs a test"]),
                "{argv:?}"
            ),
            _ => panic!("y must run it"),
        }
    }

    /// `l` opens a closed dropdown and otherwise does nothing, which is what
    /// lets the app move on to the list. It never runs the action row.
    #[test]
    fn l_in_the_sidebar_only_opens() {
        let mut p = screen();
        let action = p.controls.len() - 1;
        assert!(matches!(p.controls[action], Control::Action { .. }));
        assert!(
            matches!(p.on_sidebar(Action::Open, action), Flow::Continue),
            "l started a pull request"
        );
        assert!(!p.stale);

        assert!(matches!(p.on_sidebar(Action::Open, SORT), Flow::Dirty));
        assert!(p.open[SORT]);
        assert!(matches!(p.on_sidebar(Action::Open, SORT), Flow::Continue));
        assert!(p.open[SORT], "l closed what it had opened");
        assert!(matches!(
            p.on_sidebar(Action::Open, SORT + 1),
            Flow::Continue
        ));
    }

    /// Going back to a state fetched a moment ago shows it. Nothing arrives
    /// to say so -- the fetcher already has the answer -- so the switch has
    /// to read it.
    #[test]
    fn a_state_already_fetched_is_shown_on_the_way_back() {
        let mut p = screen();
        seed(&p, "merged", RAW);
        seed(&p, "all", "[]");
        // An error left over from the state being left is not this one's.
        p.error = Some("HTTP 502".into());
        pick(&mut p, STATE, "merged");
        assert_eq!(nums(&p), vec![70, 69, 68], "the list it already had");
        assert!(p.error.is_none(), "the last state's error was kept");
        assert_eq!(p.rows().len(), 4);

        pick(&mut p, STATE, "all");
        assert!(p.prs.is_empty());
        pick(&mut p, STATE, "merged");
        assert_eq!(
            nums(&p),
            vec![70, 69, 68],
            "nothing was read on the way back"
        );
    }

    /// `r` says so until something lands, because the list on show is the
    /// old one in the meantime.
    #[test]
    fn r_says_it_is_refreshing_until_something_lands() {
        let mut p = screen();
        seed(&p, "all", RAW);
        pick(&mut p, STATE, "all");
        p.focus(1);
        assert!(p.footer().contains("r refresh"), "{}", p.footer());
        assert!(matches!(p.on_action(Action::Key('r'), 1), Flow::Dirty));
        assert!(p.footer().contains("refreshing"), "{}", p.footer());
        assert!(!p.footer().contains("r refresh"), "{}", p.footer());
        p.sync();
        assert!(p.footer().contains("r refresh"), "{}", p.footer());
    }

    /// Changing `state` is the only control that refetches, because it is the
    /// only one that changes which pull requests exist.
    #[test]
    fn only_the_state_control_clears_the_list() {
        let mut p = screen();
        p.on_action(Action::Key('a'), 1);
        // The sort control keeps everything.
        p.open[SORT] = true;
        let sort_row = p
            .sidebar()
            .iter()
            .position(|r| r.label == "oldest")
            .expect("oldest");
        p.on_sidebar(Action::Activate, sort_row);
        assert_eq!(p.prs.len(), 3, "sorting refetched");
        assert_eq!(p.ticked.len(), 3, "sorting dropped the ticks");

        // The state control does not.
        p.open[STATE] = true;
        let state_row = p
            .sidebar()
            .iter()
            .position(|r| r.label == "closed")
            .expect("closed");
        p.on_sidebar(Action::Activate, state_row);
        assert!(p.prs.is_empty(), "changing state kept a stale list");
        assert!(
            p.ticked.is_empty(),
            "changing state kept ticks for gone rows"
        );
    }

    /// Picking from a single-select answers the question, so it closes;
    /// a multi-select stays open because you are usually choosing several.
    #[test]
    fn a_pick_closes_and_a_multi_select_stays_open() {
        let mut p = screen();
        p.open[SORT] = true;
        let row = p
            .sidebar()
            .iter()
            .position(|r| r.label == "oldest")
            .expect("oldest");
        p.on_sidebar(Action::Activate, row);
        assert!(!p.open[SORT], "a single-select stayed open");

        p.open[CREATED] = true;
        let row = p
            .sidebar()
            .iter()
            .position(|r| r.label == "someone")
            .expect("someone");
        p.on_sidebar(Action::Activate, row);
        assert!(p.open[CREATED], "a multi-select closed after one pick");
        assert_eq!(p.controls[CREATED].picked(), vec!["someone"]);
    }

    /// Run the row called `label` of a pushed menu: enter, then the `y` it
    /// asks for.
    fn run_row(menu: &mut Box<dyn Screen>, label: &str) {
        let row = menu
            .rows()
            .iter()
            .position(|r| r.label == label)
            .unwrap_or_else(|| panic!("no row called {label}"));
        assert!(matches!(menu.on_action(Action::Activate, row), Flow::Dirty));
        assert!(matches!(
            menu.on_action(Action::Key('y'), row),
            Flow::Detach(_) | Flow::DetachIn { .. }
        ));
    }

    /// A menu that ran something is followed by a re-read, for one pull
    /// request's menu and for the bulk one: what it closed is otherwise still
    /// listed as open.
    #[test]
    fn a_menu_that_ran_something_makes_the_list_ask_again() {
        let mut p = screen();
        let Flow::Push(mut menu) = p.on_action(Action::Activate, 1) else {
            panic!("enter on a pull request opens its menu");
        };
        assert!(!matches!(p.tick(), Tick::Changed), "re-asked before a `y`");
        run_row(&mut menu, "Checkout locally");
        assert!(matches!(p.tick(), Tick::Changed));
        assert!(
            !matches!(p.tick(), Tick::Changed),
            "asked once, not on every tick"
        );

        p.on_action(Action::Key('a'), 1);
        let Flow::Push(mut bulk) = p.on_action(Action::Activate, 0) else {
            panic!("enter on the count opens the bulk menu");
        };
        run_row(&mut bulk, "Close all");
        assert!(matches!(p.tick(), Tick::Changed));
    }

    /// And one that was only looked at is not. The screen used to call itself
    /// stale as the menu opened, so every look at a pull request's menu was a
    /// `gh pr list` on the way back.
    #[test]
    fn a_menu_that_was_only_looked_at_asks_for_nothing() {
        let mut p = screen();
        assert!(matches!(p.on_action(Action::Activate, 1), Flow::Push(_)));
        p.on_action(Action::Key('a'), 1);
        assert!(matches!(p.on_action(Action::Activate, 0), Flow::Push(_)));
        assert!(!p.stale);
        assert!(
            !matches!(p.tick(), Tick::Changed),
            "re-asked for a menu nothing was run from"
        );
    }
}
