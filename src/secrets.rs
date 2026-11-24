//! Secrets: the repository's, and each environment's, in one list.
//!
//! The one screen in this program whose subject cannot be read. GitHub
//! encrypts a secret's value on the way in and never gives it back -- not to
//! the API, not to `gh`, not to the Actions log -- so there is nothing for a
//! pane to show except the name, when it last changed, and how to use it. That
//! is the whole screen, and it is why there is no preview worth prefetching.
//!
//! ## Why the environments are groups rather than a second screen
//!
//! An environment secret IS a secret; the environment is just where it lives.
//! The fzf screen already listed both in one list with a glyph to tell them
//! apart, because the question you arrive with is "is `DEPLOY_TOKEN` set
//! anywhere" rather than "list the staging ones". Groups keep that and gain
//! two things the flat list could not have: `/` narrows across every scope at
//! once, and the scope of a NEW secret is decided by which group you are
//! standing in rather than by a dialog asking where it should live.
//!
//! ## Setting a value needs no text widget, and must not have one
//!
//! `gh secret set NAME` reads the value from the terminal with echo off and
//! encrypts it locally before it leaves the machine. `Flow::Detach` hands it
//! the real tty, so the value never passes through this process, is never in a
//! `String` here, and cannot end up in a redraw. A field in this program would
//! be strictly worse at the only part that matters.
//!
//! The NAME is asked for here, because `gh` takes it as an argument rather
//! than prompting for it. It is passed as typed: GitHub treats secret names
//! case-insensitively, so the upper-casing the bash version did was a
//! convention rather than a requirement, and silently rewriting what someone
//! typed is not a thing to do on their behalf. What is checked is the part
//! GitHub will refuse -- the character set, and the reserved `GITHUB_` prefix
//! -- because finding that out from a round trip is worse than being told.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tui_core::{nav::Action, Flow, Pane, Row, Screen, Tick};

use crate::gh::{Fetcher, Got, Req, IN_FLIGHT};
use crate::menu::{Act, Item, Menu, Writes};
use crate::paint::{idx, BOLD, DIM, GRY, OFF, ORG, PUR, RED};
use crate::time;

/// How long the list stays good for. The same sixty seconds the fzf screen
/// cached it for, and for the same reason: the only thing that changes a
/// secret is you, so this is about catching up after a `set` rather than about
/// watching anything.
const LIST_EVERY: Duration = Duration::from_secs(60);

/// What `gh secret list --json` hands back.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Secret {
    name: String,
    #[serde(default)]
    updated_at: Option<String>,
}

/// One group: the repository, or one environment.
struct Lane {
    /// `None` is the repository itself.
    env: Option<String>,
    secrets: Vec<Secret>,
    error: Option<String>,
    waiting: bool,
    /// Whether its rows are drawn. Kept on the lane, so that it goes where
    /// the lane goes when the environment list is read again.
    open: bool,
}

impl Lane {
    fn new(env: Option<String>) -> Self {
        Self {
            env,
            secrets: Vec::new(),
            error: None,
            waiting: true,
            open: true,
        }
    }

    fn title(&self) -> String {
        match &self.env {
            None => "\u{1f511} repository".into(),
            Some(e) => format!("\u{1f30d} {e}"),
        }
    }
}

/// Where the cursor is, derived from the rows rather than remembered.
#[derive(Debug, PartialEq, Eq)]
enum At {
    Nothing,
    Head(usize),
    Secret(usize, usize),
    New(usize),
    /// The row that says the environments could not be listed, or were not
    /// all listed. A row like any other, so it has a position -- and nothing
    /// a key could act on.
    Environments,
}

pub struct Secrets {
    repo: String,
    fetch: Arc<Fetcher>,
    lanes: Vec<Lane>,
    /// Why the environments could not be listed, when they could not. Without
    /// it a failed call looked exactly like a repository that has none.
    envs_error: Option<String>,
    /// How many environments GitHub says there are, when that is more than
    /// the one page asked for.
    envs_total: usize,
    epoch_seen: u64,
    asked: Instant,
    /// The scope a pending name belongs to -- the inner `None` is the
    /// repository. The scope itself and not the lane's position: the lanes
    /// are rebuilt when the environment list lands, which can happen while
    /// the name is being typed, and a position would then name another
    /// environment.
    asking: Option<Option<String>>,
    status: String,
    sel: usize,
    /// `gh secret set` was handed the terminal, so the next tick re-asks.
    stale: bool,
    /// Counted up by a secret's menu each time it sets or deletes, and the
    /// count the lists were last asked for at. A menu that was only looked
    /// at leaves the two equal, and nothing is asked again for it.
    writes: Writes,
    wrote_seen: u64,
}

/// The environments list, shared with the Environments screen -- same key, so
/// whichever tab is opened first pays for it.
pub fn envs_req(repo: &str) -> Req {
    Req::Json {
        path: format!("repos/{repo}/environments?per_page=100"),
    }
}

/// Where a `gh secret` command acts: this repository, and one environment of
/// it when the secret lives in one.
///
/// `--repo` on every one, because `github` is launched from anywhere and gh
/// would otherwise take the repository from the working directory. No `--env`
/// is what makes it a repository secret.
pub(crate) fn scope_args(repo: &str, env: Option<&str>) -> Vec<String> {
    let mut args = vec!["--repo".to_string(), repo.to_string()];
    if let Some(e) = env {
        args.push("--env".into());
        args.push(e.to_string());
    }
    args
}

pub(crate) fn list_req(repo: &str, env: Option<&str>) -> Req {
    let mut args = vec!["secret".to_string(), "list".into()];
    args.extend(scope_args(repo, env));
    args.push("--json".into());
    args.push("name,updatedAt".into());
    Req::Cmd { args }
}

/// What GitHub will accept as a secret name.
///
/// Checked here rather than left to the round trip, because the answer is
/// deterministic and the alternative is typing a name, handing over the
/// terminal, typing a value, and only then being told.
pub(crate) fn bad_name(name: &str) -> Option<&'static str> {
    let mut chars = name.chars();
    match chars.next() {
        None => return Some("a secret needs a name"),
        Some(c) if !c.is_ascii_alphabetic() && c != '_' => {
            return Some("a name starts with a letter or an underscore")
        }
        Some(_) => {}
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Some("letters, digits and underscores only");
    }
    // GitHub reserves the prefix for the variables it sets itself.
    if name.len() >= 7 && name[..7].eq_ignore_ascii_case("GITHUB_") {
        return Some("GITHUB_ is reserved by GitHub");
    }
    None
}

impl Secrets {
    pub fn open(repo: &str, fetch: Arc<Fetcher>) -> Self {
        fetch.request(list_req(repo, None));
        fetch.request(envs_req(repo));
        Self::new(repo, fetch)
    }

    /// The screen with nothing asked for yet.
    fn new(repo: &str, fetch: Arc<Fetcher>) -> Self {
        Self {
            repo: repo.to_string(),
            fetch,
            lanes: vec![Lane::new(None)],
            envs_error: None,
            envs_total: 0,
            epoch_seen: 0,
            asked: Instant::now(),
            asking: None,
            status: String::new(),
            sel: 0,
            stale: false,
            writes: Writes::default(),
            wrote_seen: 0,
        }
    }

    /// Read whatever has landed. Lanes appear as the environment list does, so
    /// the repository's secrets are on screen before the environments are
    /// known -- which is the common case, since most repos have none.
    fn sync(&mut self) {
        #[derive(Deserialize)]
        struct EnvNames {
            #[serde(default)]
            total_count: usize,
            // Required, so an answer of some other shape is an error to show
            // rather than a list of no environments to believe.
            environments: Vec<Named>,
        }
        #[derive(Deserialize)]
        struct Named {
            name: String,
        }
        // A call that failed is said so on a row of its own. It used to be
        // dropped, and the screen then showed the repository's secrets alone
        // -- which is also exactly what a repository with no environments
        // looks like.
        match self.fetch.get(&envs_req(&self.repo)) {
            Some(Got::Json(v)) => match EnvNames::deserialize(v.as_ref()) {
                Ok(list) => {
                    self.envs_total = list.total_count;
                    self.envs_error = None;
                    let mut names: Vec<String> =
                        list.environments.into_iter().map(|e| e.name).collect();
                    names.sort_by_key(|n| n.to_lowercase());
                    self.relane(names);
                }
                Err(e) => self.envs_error = Some(format!("unreadable environment list: {e}")),
            },
            Some(Got::Denied(d)) => self.envs_error = Some(d.fix()),
            Some(Got::Failed(e)) => self.envs_error = Some(e),
            _ => {}
        }
        for lane in &mut self.lanes {
            match self.fetch.get(&list_req(&self.repo, lane.env.as_deref())) {
                Some(Got::Json(v)) => {
                    match Vec::<Secret>::deserialize(v.as_ref()) {
                        Ok(mut s) => {
                            s.sort_by_key(|s| s.name.to_lowercase());
                            lane.secrets = s;
                            lane.error = None;
                        }
                        Err(e) => lane.error = Some(format!("unreadable secret list: {e}")),
                    }
                    lane.waiting = false;
                }
                // `gh secret list` is a plain subcommand, so there are no
                // scope headers to read -- gh says on stderr why it could not,
                // and "needs admin" is the usual answer.
                Some(Got::Failed(e)) => {
                    lane.error = Some(e);
                    lane.waiting = false;
                }
                Some(Got::Denied(d)) => {
                    lane.error = Some(d.fix());
                    lane.waiting = false;
                }
                Some(Got::Pending) | None => lane.waiting = true,
                _ => {}
            }
        }
    }

    /// One lane per environment GitHub lists NOW, in the order just sorted.
    ///
    /// Rebuilt rather than appended to. Appending kept the lane of an
    /// environment deleted in the next tab, with its `+ new secret` row still
    /// offering to set one there, and put an environment created later after
    /// every older one whatever its name. A lane that survives keeps what it
    /// had fetched and whether it was open.
    fn relane(&mut self, names: Vec<String>) {
        let mut old: Vec<Lane> = std::mem::take(&mut self.lanes);
        let mut take = |env: Option<&str>| {
            let i = old.iter().position(|l| l.env.as_deref() == env)?;
            Some(old.remove(i))
        };
        let mut next = vec![take(None).unwrap_or_else(|| Lane::new(None))];
        for name in names {
            let lane = take(Some(&name)).unwrap_or_else(|| {
                self.fetch.request(list_req(&self.repo, Some(&name)));
                Lane::new(Some(name))
            });
            next.push(lane);
        }
        self.lanes = next;
    }

    /// What the last row says about the environments, when there is
    /// something to say: the list failed, or it is one page of several.
    fn envs_note(&self) -> Option<String> {
        if let Some(e) = &self.envs_error {
            return Some(format!("could not list environments \u{2014} {e}"));
        }
        let listed = self.lanes.iter().filter(|l| l.env.is_some()).count();
        (self.envs_total > listed).then(|| {
            format!(
                "only the first {listed} of {} environments are listed",
                self.envs_total
            )
        })
    }

    fn refresh(&mut self) {
        self.fetch.refresh(envs_req(&self.repo));
        for lane in &self.lanes {
            self.fetch
                .refresh(list_req(&self.repo, lane.env.as_deref()));
        }
        self.asked = Instant::now();
    }

    fn at(&self, sel: usize) -> At {
        let mut i = 0;
        for (l, lane) in self.lanes.iter().enumerate() {
            if i == sel {
                return At::Head(l);
            }
            i += 1;
            if lane.open {
                for s in 0..lane.secrets.len() {
                    if i == sel {
                        return At::Secret(l, s);
                    }
                    i += 1;
                }
                if i == sel {
                    return At::New(l);
                }
                i += 1;
            }
            i += 1; // the spacer
        }
        if i == sel && self.envs_note().is_some() {
            return At::Environments;
        }
        At::Nothing
    }

    /// The row a lane's heading is on: the same walk as `at`, stopped early.
    fn head_row(&self, lane: usize) -> usize {
        (0..lane)
            .map(|l| {
                let inside = if self.lanes[l].open {
                    self.lanes[l].secrets.len() + 1
                } else {
                    0
                };
                1 + inside + 1
            })
            .sum()
    }

    fn settings_url(&self, sel: usize) -> String {
        match self.at(sel) {
            At::Head(l) | At::Secret(l, _) | At::New(l) => match &self.lanes[l].env {
                Some(_) => format!("https://github.com/{}/settings/environments", self.repo),
                None => format!("https://github.com/{}/settings/secrets/actions", self.repo),
            },
            At::Environments => format!("https://github.com/{}/settings/environments", self.repo),
            At::Nothing => format!("https://github.com/{}/settings/secrets/actions", self.repo),
        }
    }

    /// The two things you can do to a secret that exists.
    fn secret_menu(&self, lane: usize, idx: usize) -> Menu {
        let lane = &self.lanes[lane];
        let name = lane.secrets[idx].name.clone();
        let scope = scope_args(&self.repo, lane.env.as_deref());
        let mut set = vec![
            "gh".to_string(),
            "secret".into(),
            "set".into(),
            name.clone(),
        ];
        set.extend(scope.iter().cloned());
        let mut del = vec![
            "gh".to_string(),
            "secret".into(),
            "delete".into(),
            name.clone(),
        ];
        del.extend(scope);
        let items = vec![
            // Not a `confirm()`: `gh` stops and asks for the value, which is
            // both the prompt and the chance to back out, and a second
            // question before it would be one too many.
            Item::new("Set a new value", "gh secret set", Act::Run(set)).note(&[
                "gh asks for the value with echo off and encrypts it",
                "before it leaves this machine. Setting it again is the",
                "only way to change one -- the old value is unreadable.",
            ]),
            Item::new("Delete", "gh secret delete", Act::Run(del))
                .danger()
                .note(&[
                    "Any workflow reading it starts failing at the next",
                    "run. The value cannot be recovered or retyped from",
                    "here -- nothing has it.",
                ]),
        ];
        let kind = match &lane.env {
            Some(e) => format!("{e} secret"),
            None => "repository secret".into(),
        };
        Menu::new(kind, name, items).marks(&self.writes)
    }
}

impl Screen for Secrets {
    fn title(&self) -> String {
        "secrets".into()
    }

    fn focus(&mut self, sel: usize) {
        self.sel = sel;
    }

    fn tick(&mut self) -> Tick {
        // Only the top screen ticks, so this first runs once the menu is
        // gone. Without it a deleted secret stayed on the list, with a menu
        // behind it, until the minute was up.
        let wrote = self.writes.since(&mut self.wrote_seen);
        if wrote || self.stale {
            self.stale = false;
            self.refresh();
            return Tick::Changed;
        }
        if self.fetch.landed(&mut self.epoch_seen) {
            self.sync();
            return Tick::Changed;
        }
        if self.fetch.pending() {
            return Tick::Wake(IN_FLIGHT);
        }
        let waited = self.asked.elapsed();
        if waited >= LIST_EVERY {
            self.refresh();
            return Tick::Wake(IN_FLIGHT);
        }
        Tick::Wake(LIST_EVERY - waited)
    }

    fn rows(&mut self) -> Vec<Row> {
        let mut rows = Vec::new();
        for lane in &self.lanes {
            let mut head = Row::leaf(lane.title());
            head.group = true;
            head.expanded = lane.open;
            let head = match &lane.error {
                Some(e) => head.detail(e.clone()).badged("!", idx::RED),
                None if lane.waiting => head.detail("asking github\u{2026}"),
                None => head.detail(match lane.secrets.len() {
                    0 => "none".to_string(),
                    n => n.to_string(),
                }),
            };
            rows.push(head);
            if !lane.open {
                rows.push(Row::spacer());
                continue;
            }
            for s in &lane.secrets {
                let mut r = Row::leaf(&s.name);
                r.depth = 1;
                let when = s
                    .updated_at
                    .as_deref()
                    .and_then(time::epoch)
                    .map(|t| format!("updated {} ago", time::age((time::now() - t).max(0))))
                    .unwrap_or_default();
                rows.push(r.detail(when));
            }
            // In the group, so the scope of a new secret is wherever the
            // cursor already is. No dialog asks where it should live.
            let mut new = Row::leaf(match &lane.env {
                Some(e) => format!("+ new secret in {e}"),
                None => "+ new repository secret".into(),
            });
            new.depth = 1;
            rows.push(new.detail("gh secret set"));
            rows.push(Row::spacer());
        }
        if let Some(note) = self.envs_note() {
            let row = Row::leaf(note).detail(String::new());
            rows.push(if self.envs_error.is_some() {
                row.badged("!", idx::RED)
            } else {
                row
            });
        }
        rows
    }

    /// Where a group closed from inside it leaves the cursor: on its heading.
    /// `focus` keeps this current the rest of the time.
    fn initial_sel(&self) -> usize {
        self.sel
    }

    fn pane(&mut self, sel: usize, cols: u16, _rows: u16) -> Pane {
        let mut lines = Vec::new();
        let write_only = |lines: &mut Vec<String>| {
            lines.push(format!(
                "{GRY}Values are write-only. GitHub never returns one{OFF}"
            ));
            lines.push(format!(
                "{GRY}-- not to the API, not to the CLI, not to a log.{OFF}"
            ));
        };
        match self.at(sel) {
            At::Nothing => {}
            At::Environments => {
                lines.push(format!("{BOLD}\u{1f30d} environments{OFF}"));
                lines.push(crate::paint::rule(cols));
                match &self.envs_error {
                    Some(e) => {
                        lines.push(format!("{RED}{e}{OFF}"));
                        lines.push(String::new());
                        lines.push(format!(
                            "{DIM}So this is not every secret: an environment's{OFF}"
                        ));
                        lines.push(format!(
                            "{DIM}are listed under it, and the environments{OFF}"
                        ));
                        lines.push(format!("{DIM}themselves could not be. r asks again.{OFF}"));
                    }
                    None => {
                        lines.push(format!(
                            "{DIM}GitHub has {} environments and one page of{OFF}",
                            self.envs_total
                        ));
                        lines.push(format!(
                            "{DIM}them is here, so a secret in one of the rest{OFF}"
                        ));
                        lines.push(format!(
                            "{DIM}is not on this screen. o opens them all.{OFF}"
                        ));
                    }
                }
            }
            At::Head(l) => {
                let lane = &self.lanes[l];
                lines.push(format!("{BOLD}{}{OFF}", lane.title()));
                match &lane.env {
                    Some(e) => lines.push(format!(
                        "{GRY}available to a job that declares environment: {e}{OFF}"
                    )),
                    None => {
                        lines.push(format!("{GRY}available to every workflow in the repo{OFF}"))
                    }
                }
                lines.push(crate::paint::rule(cols));
                if let Some(e) = &lane.error {
                    lines.push(format!("{RED}{e}{OFF}"));
                    // Only where gh's own words point that way. Said after
                    // every failure, it told someone whose network was down
                    // to go and ask for admin.
                    if e.contains("403") || e.contains("404") {
                        lines.push(String::new());
                        lines.push(format!(
                            "{DIM}Listing secrets needs admin on the repository.{OFF}"
                        ));
                    }
                    return Pane { image: None, lines };
                }
                // Not a count until there is an answer to count: "0 here"
                // over a list still on its way reads as a finding.
                if lane.waiting {
                    lines.push(format!("{DIM}asking github\u{2026}{OFF}"));
                } else {
                    lines.push(format!("{} here", lane.secrets.len()));
                }
                lines.push(String::new());
                write_only(&mut lines);
            }
            At::Secret(l, s) => {
                let lane = &self.lanes[l];
                let secret = &lane.secrets[s];
                lines.push(format!("{BOLD}{}{OFF}", secret.name));
                match &lane.env {
                    Some(e) => lines.push(format!("{GRY}environment secret \u{b7} {PUR}{e}{OFF}")),
                    None => lines.push(format!("{GRY}repository secret{OFF}")),
                }
                lines.push(crate::paint::rule(cols));
                write_only(&mut lines);
                lines.push(format!(
                    "{GRY}To change one, set it again -- that is the only way.{OFF}"
                ));
                lines.push(String::new());
                lines.push(format!("{DIM}Used in a workflow as{OFF}"));
                lines.push(format!("  {ORG}${{{{ secrets.{} }}}}{OFF}", secret.name));
                if let Some(e) = &lane.env {
                    lines.push(String::new());
                    lines.push(format!(
                        "{DIM}which resolves only in a job that declares{OFF}"
                    ));
                    lines.push(format!("  {ORG}environment: {e}{OFF}"));
                }
            }
            At::New(l) => {
                let lane = &self.lanes[l];
                lines.push(format!("{BOLD}a new secret{OFF}"));
                match &lane.env {
                    Some(e) => lines.push(format!("{GRY}in the {e} environment{OFF}")),
                    None => lines.push(format!("{GRY}on the repository{OFF}")),
                }
                lines.push(crate::paint::rule(cols));
                lines.push(format!(
                    "{DIM}enter asks for the name. Then gh takes the{OFF}"
                ));
                lines.push(format!(
                    "{DIM}terminal and asks for the value with echo{OFF}"
                ));
                lines.push(format!(
                    "{DIM}off, and encrypts it before it leaves this{OFF}"
                ));
                lines.push(format!("{DIM}machine -- so the value never passes{OFF}"));
                lines.push(format!("{DIM}through this program at all.{OFF}"));
                lines.push(String::new());
                lines.push(format!(
                    "{GRY}A name already here replaces that secret's value.{OFF}"
                ));
            }
        }
        Pane { image: None, lines }
    }

    fn on_action(&mut self, action: Action, sel: usize) -> Flow {
        self.status.clear();
        match (action, self.at(sel)) {
            (Action::Activate, At::Head(l)) => {
                self.lanes[l].open = !self.lanes[l].open;
                Flow::Dirty
            }
            // `l` is a direction: it opens a closed group and does nothing
            // else. Matched together with enter it also CLOSED an open one,
            // and since that is never "nothing happened" the pane beside the
            // list could not be reached from a heading at all.
            (Action::Open, At::Head(l)) if !self.lanes[l].open => {
                self.lanes[l].open = true;
                Flow::Dirty
            }
            (Action::Close, At::Head(l)) => {
                self.lanes[l].open = false;
                Flow::Dirty
            }
            // The rows under the cursor are about to go, so say where it
            // belongs. Left where it was, it sat on whatever moved up into
            // that position -- the next group's heading, or one of its
            // secrets.
            (Action::Close, At::Secret(l, _) | At::New(l)) => {
                self.lanes[l].open = false;
                self.sel = self.head_row(l);
                Flow::Retarget
            }
            (Action::Activate, At::Secret(l, s)) => {
                // Either item in there changes what this list should say,
                // and the menu counts the one that runs: see `tick`.
                Flow::Push(Box::new(self.secret_menu(l, s)))
            }
            (Action::Activate, At::New(l)) => {
                self.asking = Some(self.lanes[l].env.clone());
                Flow::Ask {
                    label: match &self.lanes[l].env {
                        Some(e) => format!("name of the new {e} secret"),
                        None => "name of the new repository secret".into(),
                    },
                    initial: String::new(),
                }
            }
            (Action::Key('o'), _) => {
                self.status = crate::browse(&self.settings_url(sel));
                Flow::Dirty
            }
            (Action::Key('r'), _) => {
                self.refresh();
                Flow::Dirty
            }
            _ => Flow::Continue,
        }
    }

    fn answered(&mut self, text: Option<String>) -> Flow {
        let Some(scope) = self.asking.take() else {
            return Flow::Continue;
        };
        let Some(name) = text.map(|t| t.trim().to_string()) else {
            return Flow::Dirty;
        };
        if let Some(why) = bad_name(&name) {
            self.status = why.into();
            return Flow::Dirty;
        }
        // The environment can have gone from the list while the name was
        // being typed, and a secret is not something to aim at one that may
        // not be there.
        if let Some(e) = &scope {
            if !self.lanes.iter().any(|l| l.env.as_ref() == Some(e)) {
                self.status = format!("{e} is no longer one of the environments listed");
                return Flow::Dirty;
            }
        }
        let mut argv = vec!["gh".to_string(), "secret".into(), "set".into(), name];
        argv.extend(scope_args(&self.repo, scope.as_deref()));
        // The list will not know about it until it is asked again, which the
        // first tick after the terminal comes back does.
        self.stale = true;
        Flow::Detach(argv)
    }

    fn footer(&self) -> String {
        if !self.status.is_empty() {
            return self.status.clone();
        }
        match self.at(self.sel) {
            At::Head(_) => "enter open/close \u{b7} o browser \u{b7} r refresh".into(),
            At::Secret(..) => "enter set or delete \u{b7} o browser \u{b7} r refresh".into(),
            At::New(_) => "enter names it, then gh asks for the value".into(),
            At::Environments | At::Nothing => "o browser \u{b7} r refresh".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menu::check;

    fn screen() -> Secrets {
        let mut s = Secrets::open("roest1/dotfiles", Arc::new(Fetcher::new("")));
        s.lanes[0].waiting = false;
        s.lanes[0].secrets = vec![
            Secret {
                name: "STATS_TOKEN".into(),
                updated_at: Some("2026-10-20T00:00:00Z".into()),
            },
            Secret {
                name: "NPM_TOKEN".into(),
                updated_at: None,
            },
        ];
        s.lanes.push(Lane {
            env: Some("staging".into()),
            secrets: vec![Secret {
                name: "DEPLOY_KEY".into(),
                updated_at: None,
            }],
            error: None,
            waiting: false,
            open: true,
        });
        s
    }

    /// The scope comes from the group the cursor is in, which is the whole
    /// reason the environments are groups: there is no dialog asking where a
    /// new secret should live.
    #[test]
    fn a_new_secret_takes_the_scope_of_the_group_it_is_in() {
        let mut s = screen();
        // repository: head, two secrets, new, spacer -> 0..4
        assert_eq!(s.at(3), At::New(0));
        // staging: head, one secret, new, spacer -> 5..8
        assert_eq!(s.at(7), At::New(1));
        assert!(matches!(s.on_action(Action::Activate, 7), Flow::Ask { .. }));
        match s.answered(Some("DEPLOY_TOKEN".into())) {
            Flow::Detach(argv) => assert_eq!(
                argv,
                vec![
                    "gh",
                    "secret",
                    "set",
                    "DEPLOY_TOKEN",
                    "--repo",
                    "roest1/dotfiles",
                    "--env",
                    "staging"
                ]
            ),
            f => panic!("expected a detach, got {}", flow_name(&f)),
        }
    }

    /// A repository secret gets no `--env`, which is what makes it a
    /// repository secret rather than one in an environment called nothing.
    #[test]
    fn a_repository_secret_is_scoped_to_the_repo_alone() {
        let mut s = screen();
        s.on_action(Action::Activate, 3);
        match s.answered(Some("  NPM_TOKEN  ".into())) {
            Flow::Detach(argv) => {
                assert!(!argv.iter().any(|a| a == "--env"), "{argv:?}");
                assert_eq!(argv[3], "NPM_TOKEN", "trimmed");
            }
            f => panic!("expected a detach, got {}", flow_name(&f)),
        }
    }

    /// Names GitHub will refuse are refused here, where saying so costs
    /// nothing. The alternative is typing a name, handing over the terminal,
    /// typing a VALUE, and being told then.
    #[test]
    fn a_name_github_would_reject_never_reaches_gh() {
        for (name, why) in [
            ("", "needs a name"),
            ("2FA_TOKEN", "starts with"),
            ("MY-TOKEN", "underscores only"),
            ("MY TOKEN", "underscores only"),
            ("GITHUB_TOKEN", "reserved"),
            ("github_token", "reserved"),
        ] {
            let mut s = screen();
            s.on_action(Action::Activate, 3);
            let flow = s.answered(Some(name.to_string()));
            assert!(
                !matches!(flow, Flow::Detach(_)),
                "{name:?} was handed to gh"
            );
            assert!(s.footer().contains(why), "{name:?}: {}", s.footer());
        }
        // And one that is fine.
        let mut s = screen();
        s.on_action(Action::Activate, 3);
        assert!(matches!(
            s.answered(Some("_PRIVATE_2".into())),
            Flow::Detach(_)
        ));
    }

    /// An answer arriving when nothing was asked must not set a secret named
    /// after whatever was typed.
    #[test]
    fn an_unexpected_answer_does_nothing() {
        let mut s = screen();
        assert!(matches!(s.answered(Some("X".into())), Flow::Continue));
    }

    /// A collapsed group hides its secrets AND its new-secret row, so the
    /// positions after it have to move by the whole block.
    #[test]
    fn collapsing_a_group_moves_everything_after_it() {
        let mut s = screen();
        s.on_action(Action::Close, 0);
        assert_eq!(s.at(0), At::Head(0));
        assert_eq!(s.at(1), At::Nothing, "the spacer after the closed group");
        assert_eq!(s.at(2), At::Head(1), "staging moved up from row 5");
        assert_eq!(s.at(3), At::Secret(1, 0));
    }

    /// The rows and the positions are two readings of one list, so they have
    /// to agree -- `at` walking differently from `rows` is how a delete lands
    /// on the wrong secret.
    #[test]
    fn the_rows_and_the_positions_agree() {
        let mut s = screen();
        rows_and_positions_agree(&mut s);
        // With a group closed, and with nothing in any of them.
        s.on_action(Action::Close, 0);
        rows_and_positions_agree(&mut s);
        let mut s = quiet();
        rows_and_positions_agree(&mut s);
    }

    /// `o` goes to the page that holds what you were looking at: the repo's
    /// Actions secrets, or the environments page for an environment's.
    #[test]
    fn the_browser_key_follows_the_scope() {
        let s = screen();
        assert!(s.settings_url(1).ends_with("/settings/secrets/actions"));
        assert!(s.settings_url(6).ends_with("/settings/environments"));
    }

    /// A lane that could not be listed says so on its own row and leaves the
    /// others alone -- one environment you cannot read is not an empty screen.
    #[test]
    fn one_lane_failing_does_not_empty_the_screen() {
        let mut s = screen();
        s.lanes[1].error = Some("gh: Not Found (HTTP 404)".into());
        s.lanes[1].secrets.clear();
        let rows = s.rows();
        assert!(rows.iter().any(|r| r.label.contains("STATS_TOKEN")));
        let head = rows
            .iter()
            .find(|r| r.label.contains("staging"))
            .expect("head");
        assert_eq!(head.badges.first().map(|b| b.0.as_str()), Some("!"));
    }

    /// A screen that has asked for nothing, so an answer put in by hand is
    /// not raced by the real one.
    fn quiet() -> Secrets {
        Secrets::new("roest1/dotfiles", Arc::new(Fetcher::new("")))
    }

    fn json(text: &str) -> Got {
        Got::Json(Arc::new(serde_json::from_str(text).expect("parses")))
    }

    /// The environments list, and an empty secret list for each one named, so
    /// that reading it asks GitHub for nothing.
    fn seed_envs(s: &Secrets, total: usize, names: &[&str]) {
        let list: Vec<String> = names
            .iter()
            .map(|n| format!(r#"{{"name":"{n}"}}"#))
            .collect();
        s.fetch.seed(
            envs_req(&s.repo),
            json(&format!(
                r#"{{"total_count":{total},"environments":[{}]}}"#,
                list.join(",")
            )),
        );
        for n in names {
            s.fetch.seed(list_req(&s.repo, Some(n)), json("[]"));
        }
    }

    fn titles(s: &Secrets) -> Vec<Option<&str>> {
        s.lanes.iter().map(|l| l.env.as_deref()).collect()
    }

    /// Every row is something `at` can name, and the one row that is not a
    /// lane -- the note about the environments -- is nothing a key acts on.
    fn rows_and_positions_agree(s: &mut Secrets) {
        let rows = s.rows();
        for (i, row) in rows.iter().enumerate() {
            match s.at(i) {
                At::Head(l) => assert_eq!(row.label, s.lanes[l].title()),
                At::Secret(l, x) => assert_eq!(row.label, s.lanes[l].secrets[x].name),
                At::New(_) => assert!(row.label.starts_with("+ new"), "{}", row.label),
                At::Environments => {
                    assert!(row.label.contains("environments"), "{}", row.label);
                    assert!(matches!(s.on_action(Action::Activate, i), Flow::Continue));
                    assert!(matches!(s.on_action(Action::Close, i), Flow::Continue));
                }
                At::Nothing => assert!(!row.selectable, "{} is not a spacer", row.label),
            }
        }
        assert_eq!(s.at(rows.len()), At::Nothing, "past the last row");
    }

    /// The menu behind a secret can delete it or set it again, so coming back
    /// from one that did re-asks. It used not to, and the deleted secret
    /// stayed on the list with a menu still behind it.
    #[test]
    fn a_secrets_menu_that_ran_something_makes_the_list_ask_again() {
        let mut s = screen();
        let Flow::Push(mut menu) = s.on_action(Action::Activate, 1) else {
            panic!("enter on a secret opens its menu");
        };
        // Delete asks first, and asking is not running.
        assert!(matches!(menu.on_action(Action::Activate, 1), Flow::Dirty));
        assert!(!matches!(s.tick(), Tick::Changed), "re-asked before a `y`");
        assert!(matches!(
            menu.on_action(Action::Key('y'), 1),
            Flow::Detach(_)
        ));
        assert!(
            matches!(s.tick(), Tick::Changed),
            "the list was not re-asked after the delete"
        );
        assert!(
            !matches!(s.tick(), Tick::Changed),
            "asked once, not on every tick"
        );
    }

    /// And one that was only looked at asks for nothing. The screen used to
    /// call itself stale as the menu opened, so every look at a secret's menu
    /// was a `gh secret list` per scope on the way back.
    #[test]
    fn a_secrets_menu_that_was_only_looked_at_asks_for_nothing() {
        let mut s = screen();
        assert!(matches!(s.on_action(Action::Activate, 1), Flow::Push(_)));
        assert!(!s.stale);
        assert!(
            !matches!(s.tick(), Tick::Changed),
            "re-asked for a menu nothing was run from"
        );
    }

    /// `h` from inside a group closes it and takes the cursor to its heading.
    /// Left at the same position it was on the NEXT group's heading, or on
    /// one of that group's secrets.
    #[test]
    fn closing_a_group_from_inside_it_puts_the_cursor_on_its_heading() {
        // From a secret in the second group, with the first still open.
        let mut s = screen();
        assert_eq!(s.at(6), At::Secret(1, 0));
        assert!(matches!(s.on_action(Action::Close, 6), Flow::Retarget));
        assert_eq!(s.at(s.initial_sel()), At::Head(1));
        assert!(!s.lanes[1].open);
        // From the first group's `+ new` row.
        let mut s = screen();
        assert_eq!(s.at(3), At::New(0));
        assert!(matches!(s.on_action(Action::Close, 3), Flow::Retarget));
        assert_eq!(s.at(s.initial_sel()), At::Head(0));
        // And with the group above CLOSED, which moves every row after it.
        let mut s = screen();
        s.on_action(Action::Close, 0);
        assert_eq!(s.at(3), At::Secret(1, 0));
        assert!(matches!(s.on_action(Action::Close, 3), Flow::Retarget));
        assert_eq!(s.initial_sel(), 2);
        assert_eq!(s.at(2), At::Head(1));
    }

    /// The heading's row is read off the same walk as everything else.
    #[test]
    fn a_heading_is_where_the_walk_says_it_is() {
        let mut s = screen();
        for l in 0..s.lanes.len() {
            assert_eq!(s.at(s.head_row(l)), At::Head(l));
        }
        s.lanes[0].open = false;
        assert_eq!(s.at(s.head_row(1)), At::Head(1));
    }

    /// `l` opens a closed group and otherwise does nothing, which is what
    /// lets it move on into the pane. It never closes one and never starts
    /// naming a secret; enter does those.
    #[test]
    fn l_opens_a_closed_group_and_otherwise_moves_on() {
        let mut s = screen();
        assert!(matches!(s.on_action(Action::Open, 0), Flow::Continue));
        assert!(s.lanes[0].open, "l closed an open group");
        s.on_action(Action::Close, 0);
        assert!(matches!(s.on_action(Action::Open, 0), Flow::Dirty));
        assert!(s.lanes[0].open, "l did not open a closed group");
        assert!(matches!(s.on_action(Action::Open, 1), Flow::Continue));
        assert!(matches!(s.on_action(Action::Open, 3), Flow::Continue));
        assert!(s.asking.is_none(), "l asked for a secret's name");
        // Enter still toggles a heading.
        assert!(matches!(s.on_action(Action::Activate, 0), Flow::Dirty));
        assert!(!s.lanes[0].open);
    }

    /// An environments call that failed, was refused, or came back as
    /// something else is a row saying so. Dropped, it left the repository's
    /// secrets on their own, which is what a repo with no environments shows.
    #[test]
    fn environments_that_could_not_be_listed_are_said_so() {
        let denied = crate::scopes::Denied {
            accepted: vec!["repo".into()],
            suggest: "repo".into(),
        };
        for (got, says) in [
            (
                Got::Failed("gh: Server Error (HTTP 502)".into()),
                "HTTP 502",
            ),
            (Got::Denied(denied), "gh auth refresh"),
            (json(r#"{"message":"hello"}"#), "unreadable"),
        ] {
            let mut s = quiet();
            s.fetch.seed(envs_req(&s.repo), got);
            s.sync();
            let rows = s.rows();
            let last = rows.len() - 1;
            assert!(
                rows[last].label.contains("could not list environments")
                    && rows[last].label.contains(says),
                "{says}: {:?}",
                rows[last].label
            );
            assert_eq!(s.at(last), At::Environments);
            assert!(s.pane(last, 60, 40).lines.join("\n").contains(says));
            rows_and_positions_agree(&mut s);
        }
        // An answer still on its way is not a failure, and says nothing.
        let mut s = quiet();
        s.fetch.seed(envs_req(&s.repo), Got::Pending);
        s.sync();
        assert!(s.envs_note().is_none());
        rows_and_positions_agree(&mut s);
    }

    /// The lanes are the environments GitHub lists now, in name order: one
    /// deleted elsewhere goes, and one created later sorts into place. A lane
    /// that stays keeps whether it was open.
    #[test]
    fn the_lanes_follow_the_environment_list() {
        let mut s = quiet();
        seed_envs(&s, 2, &["staging", "Production"]);
        s.sync();
        assert_eq!(titles(&s), [None, Some("Production"), Some("staging")]);
        s.lanes[2].open = false;
        seed_envs(&s, 2, &["staging", "canary"]);
        s.sync();
        assert_eq!(titles(&s), [None, Some("canary"), Some("staging")]);
        let open: Vec<bool> = s.lanes.iter().map(|l| l.open).collect();
        assert_eq!(open, [true, true, false], "staging forgot it was closed");
        rows_and_positions_agree(&mut s);
        seed_envs(&s, 0, &[]);
        s.sync();
        assert_eq!(titles(&s), [None]);
    }

    /// A name typed for an environment that left the list meanwhile is not
    /// handed to gh, and one typed for another lane's position cannot be: the
    /// answer carries the scope it was asked for.
    #[test]
    fn a_name_is_for_the_scope_it_was_asked_in_even_if_the_lanes_move() {
        let mut s = quiet();
        seed_envs(&s, 2, &["prod", "staging"]);
        s.sync();
        // repository: head, new, spacer; prod: head, new, spacer; staging: head, new
        assert_eq!(s.at(7), At::New(2));
        assert!(matches!(s.on_action(Action::Activate, 7), Flow::Ask { .. }));
        // `canary` now sorts ahead of it, so staging is no longer lane 2.
        seed_envs(&s, 3, &["prod", "staging", "canary"]);
        s.sync();
        match s.answered(Some("DEPLOY_TOKEN".into())) {
            Flow::Detach(argv) => assert_eq!(argv.last().map(String::as_str), Some("staging")),
            f => panic!("expected a detach, got {}", flow_name(&f)),
        }
        // And when the environment itself has gone, nothing runs.
        assert!(matches!(
            s.on_action(Action::Activate, 10),
            Flow::Ask { .. }
        ));
        seed_envs(&s, 2, &["prod", "canary"]);
        s.sync();
        let flow = s.answered(Some("DEPLOY_TOKEN".into()));
        assert!(!matches!(flow, Flow::Detach(_)), "set in a deleted lane");
        assert!(s.footer().contains("no longer"), "{}", s.footer());
    }

    /// One page of environments is said to be one page.
    #[test]
    fn a_cut_environment_list_says_how_many_are_missing() {
        let mut s = quiet();
        seed_envs(&s, 140, &["prod", "staging"]);
        s.sync();
        let rows = s.rows();
        let last = rows.len() - 1;
        assert!(
            rows[last].label.contains("first 2 of 140"),
            "{:?}",
            rows[last].label
        );
        assert_eq!(s.at(last), At::Environments);
        rows_and_positions_agree(&mut s);
    }

    /// A lane still waiting has no count, and only an error that reads like a
    /// refusal is put down to permissions.
    #[test]
    fn a_heading_pane_does_not_count_or_blame_before_it_knows() {
        let mut s = screen();
        s.lanes[0].secrets.clear();
        s.lanes[0].waiting = true;
        let text = s.pane(0, 60, 40).lines.join("\n");
        assert!(text.contains("asking github"), "{text}");
        assert!(!text.contains("0 here"), "{text}");
        s.lanes[0].waiting = false;
        assert!(s.pane(0, 60, 40).lines.join("\n").contains("0 here"));

        s.lanes[0].error = Some("could not run gh: connection reset".into());
        let text = s.pane(0, 60, 40).lines.join("\n");
        assert!(text.contains("connection reset"), "{text}");
        assert!(!text.contains("needs admin"), "{text}");
        s.lanes[0].error = Some("HTTP 403: Resource not accessible".into());
        assert!(s.pane(0, 60, 40).lines.join("\n").contains("needs admin"));
    }

    /// Everything a secret's menu can run names the repository, and the one
    /// item that changes something without gh stopping to ask first is
    /// confirmed. `gh secret set` is the exception by design: it stops for
    /// the value, which is both the prompt and the way out.
    #[test]
    fn every_menu_item_names_the_repo_and_asks_before_it_changes_anything() {
        let s = screen();
        for (lane, idx) in [(0, 0), (1, 0)] {
            let menu = s.secret_menu(lane, idx);
            for item in menu.items() {
                let Act::Run(argv) = &item.act else {
                    panic!("{:?} is not a plain command", item.label);
                };
                assert!(
                    check::names_repo(argv, "roest1/dotfiles"),
                    "{:?} names no repo: {argv:?}",
                    item.label
                );
                assert!(
                    check::asks_first(item.asks, argv),
                    "{:?} runs on one keypress: {argv:?}",
                    item.label
                );
            }
        }
    }

    fn flow_name(f: &Flow) -> &'static str {
        match f {
            Flow::Continue => "Continue",
            Flow::Dirty => "Dirty",
            Flow::Push(_) => "Push",
            Flow::Pop => "Pop",
            Flow::Ask { .. } => "Ask",
            Flow::Retarget => "Retarget",
            Flow::Detach(_) => "Detach",
            Flow::DetachIn { .. } => "DetachIn",
            Flow::Quit => "Quit",
        }
    }
}
