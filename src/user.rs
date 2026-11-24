//! The user level: who you are, what this token may do, and your keys.
//!
//! The BOTTOM of the stack, always -- including when `github` is run inside a
//! repo. That is the whole answer to "I want to reach my settings even when I
//! opened this in a repo": there is one root, and standing in a repo seeds you
//! deeper into it rather than giving you a different one. `q` walks up from a
//! workflow run to the repo list to here. No jump key, because a jump key would
//! be a second way to express what the stack already says.
//!
//! ## Why the Account tab leads with scopes
//!
//! Most of what GitHub calls "user settings" is not in the API, and the parts
//! that are need scopes a `repo`-and-`read:org` token does not have. A screen
//! that just showed empty lists would be lying about an account that has keys.
//! So the first thing this says is what the token can do, read off the headers
//! of whatever call last ran -- see `crate::scopes` -- and every denial carries
//! the one command that fixes it.
//!
//! ## What is deliberately absent
//!
//! Personal access tokens and 2FA have NO API, at any scope. They are not
//! pending work and should not appear as empty rows; the Account tab says so in
//! one line instead, because "this screen cannot show you that" is information
//! and a blank list is not.

use std::sync::Arc;

use serde::Deserialize;
use tui_core::{nav::Action, Flow, Host, Pane, Row, Screen, Tick};

use crate::gh::{Fetcher, Got, Req, PAGE};
use crate::orgs::{cut_row, OrgList};
use crate::paint::{idx, BOLD, DIM, GRN, GRY, OFF, ORG, PUR, RED, YEL};
use crate::root::Root;

/// Build the user-level screen.
///
/// `root` is the Repos tab, already focused on the repo the shell is standing
/// in if there is one -- which is what makes `github` in a repo feel like it
/// opened where you were standing while still having the account underneath
/// it.
///
/// `login` labels the strip. `None` leaves the strip without a label rather
/// than inventing one: a made-up name sits exactly where a real one would and
/// reads as whose account this is.
pub fn screen(fetch: Arc<Fetcher>, root: Root, login: Option<&str>) -> Host {
    Host::new(
        login.map(str::to_string),
        // Glyphs on the tabs, which is where they earn the most: the strip is
        // the thing you look at to navigate, and a shape is faster to find
        // than a word. The set is deliberately small and each one is a noun
        // you would draw -- a folder, a building, a person, a key.
        vec![
            ("\u{1f4c2} Repos".into(), Box::new(root) as Box<dyn Screen>),
            (
                "\u{1f3e2} Orgs".into(),
                Box::new(OrgList::new(Arc::clone(&fetch))),
            ),
            (
                "\u{1f464} Account".into(),
                Box::new(Account::new(Arc::clone(&fetch))),
            ),
            ("\u{1f511} Keys".into(), Box::new(Keys::new(fetch))),
        ],
    )
}

/// The login, read from `gh`'s config file and never from the network.
///
/// This runs in front of the first frame, so it may only ask what is already
/// on this disk. `gh config get` reads a local file. `gh api user` is a round
/// trip, and so is `gh auth status`, which validates the token before it
/// prints anything -- either one holds a blank terminal for as long as the
/// network takes, and on a dead link that is until it times out. The Account
/// tab fetches the full profile afterwards, off the main thread.
///
/// `None` when the config cannot say, and there is no stand-in: the answer is
/// handed to the pull request filters as "me", and any word put here in its
/// place -- `github` was one -- is somebody's real login.
///
/// A token in the environment is one of the cases it cannot say. `gh` uses
/// `GH_TOKEN` or `GITHUB_TOKEN` in preference to its stored credentials, so
/// the stored login is then not necessarily who the calls run as.
pub fn login() -> Option<String> {
    if token_in_env(|k| std::env::var(k).ok()) {
        return None;
    }
    let out = std::process::Command::new("gh")
        .args(["config", "get", "user", "-h", "github.com"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    as_login(&String::from_utf8_lossy(&out.stdout))
}

fn token_in_env(var: impl Fn(&str) -> Option<String>) -> bool {
    ["GH_TOKEN", "GITHUB_TOKEN"]
        .iter()
        .any(|k| var(k).is_some_and(|v| !v.is_empty()))
}

/// What `gh config get user` printed, if that is a login.
///
/// Checked rather than trusted, because whatever comes back is used as a
/// name: letters, digits and hyphens, plus the underscore a managed account
/// carries. Anything else is a message, not a person.
fn as_login(printed: &str) -> Option<String> {
    let s = printed.trim();
    let ok = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    ok.then(|| s.to_string())
}

#[derive(Debug, Clone, Deserialize)]
struct Profile {
    login: String,
    name: Option<String>,
    bio: Option<String>,
    company: Option<String>,
    location: Option<String>,
    #[serde(default)]
    public_repos: u64,
    #[serde(default)]
    total_private_repos: u64,
    #[serde(default)]
    followers: u64,
    #[serde(default)]
    following: u64,
    created_at: Option<String>,
}

// ─── Account ──────────────────────────────────────────────────────

pub struct Account {
    fetch: Arc<Fetcher>,
    profile: Option<Profile>,
    epoch_seen: u64,
    error: Option<String>,
    sel: usize,
}

/// What the rows on this tab are. Resolved once, as everywhere else here, so
/// `pane` and `at` cannot disagree about which row is which.
enum AccountRow {
    Profile,
    Scopes,
    NoApi,
}

/// What is known about the token's scopes, which is not the same question as
/// whether it has any.
enum Scopes {
    Granted(crate::scopes::Granted),
    /// No response has come back yet.
    Asking,
    /// GitHub answered and named none. A fine-grained or app token sends no
    /// `X-OAuth-Scopes` header at all, so for those this is the final answer
    /// and "asking" would be shown for the life of the session.
    Unreported,
    /// The call failed, so nothing was learned either way.
    Unknown,
}

impl Account {
    fn profile_req() -> Req {
        Req::Json {
            path: "user".into(),
        }
    }

    fn new(fetch: Arc<Fetcher>) -> Self {
        fetch.request(Self::profile_req());
        Self {
            fetch,
            profile: None,
            epoch_seen: 0,
            error: None,
            sel: 0,
        }
    }

    fn at(&self, sel: usize) -> AccountRow {
        match sel {
            0 => AccountRow::Profile,
            1 => AccountRow::Scopes,
            _ => AccountRow::NoApi,
        }
    }

    /// Read off the profile request, because that is the one call this tab
    /// knows it made: once it has an answer, the header has had its chance.
    fn scopes(&self) -> Scopes {
        let granted = self.fetch.granted();
        if !granted.is_empty() {
            return Scopes::Granted(granted);
        }
        match self.fetch.get(&Self::profile_req()) {
            Some(Got::Json(_)) | Some(Got::Denied(_)) => Scopes::Unreported,
            Some(Got::Failed(_)) => Scopes::Unknown,
            _ => Scopes::Asking,
        }
    }

    fn sync(&mut self) {
        match self.fetch.get(&Self::profile_req()) {
            Some(Got::Json(v)) => match Profile::deserialize(v.as_ref()) {
                Ok(p) => {
                    self.profile = Some(p);
                    self.error = None;
                }
                Err(e) => self.error = Some(format!("unreadable profile: {e}")),
            },
            Some(Got::Denied(d)) => self.error = Some(d.fix()),
            Some(Got::Failed(e)) => self.error = Some(e),
            _ => {}
        }
    }
}

impl Screen for Account {
    fn title(&self) -> String {
        "account".into()
    }

    fn focus(&mut self, sel: usize) {
        self.sel = sel;
    }

    fn tick(&mut self) -> Tick {
        if self.fetch.landed(&mut self.epoch_seen) {
            self.sync();
            return Tick::Changed;
        }
        // Without this the tab goes idle on "fetching..." with the profile
        // still on its way, and draws it on the next keypress.
        self.fetch.waiting()
    }

    fn rows(&mut self) -> Vec<Row> {
        let scope_detail = match self.scopes() {
            Scopes::Granted(g) => format!("{} granted", g.list().len()),
            Scopes::Asking => "asking\u{2026}".to_string(),
            Scopes::Unreported => "none reported".to_string(),
            Scopes::Unknown => "unknown".to_string(),
        };
        // A profile that failed is not one still being fetched, and the row
        // is what you see without selecting it.
        let profile = match (&self.profile, &self.error) {
            (Some(p), _) => Row::leaf("profile").detail(p.login.clone()),
            (None, Some(_)) => Row::leaf("profile").badged("error", idx::RED),
            (None, None) => Row::leaf("profile").detail("fetching\u{2026}"),
        };
        vec![
            profile,
            Row::leaf("token scopes").detail(scope_detail),
            Row::leaf("not available here").detail("tokens, 2FA"),
        ]
    }

    fn pane(&mut self, sel: usize, cols: u16, _rows: u16) -> Pane {
        let mut lines = Vec::new();
        match self.at(sel) {
            AccountRow::Profile => match (&self.profile, &self.error) {
                (_, Some(e)) => {
                    lines.push(format!("{RED}{e}{OFF}"));
                }
                (None, None) => lines.push(format!("{DIM}fetching your profile\u{2026}{OFF}")),
                (Some(p), _) => {
                    lines.push(format!(
                        "{BOLD}{}{OFF}",
                        p.name.as_deref().unwrap_or(&p.login)
                    ));
                    lines.push(format!("{GRY}{}{OFF}", p.login));
                    if let Some(bio) = &p.bio {
                        lines.push(String::new());
                        lines.push(bio.clone());
                    }
                    lines.push(String::new());
                    for (k, v) in [
                        ("company", p.company.clone()),
                        ("location", p.location.clone()),
                        (
                            "joined",
                            p.created_at
                                .as_ref()
                                .and_then(|c| c.get(..10).map(str::to_string)),
                        ),
                    ] {
                        if let Some(v) = v {
                            lines.push(format!("{k:10} {v}"));
                        }
                    }
                    lines.push(String::new());
                    lines.push(format!(
                        "{:10} {} public, {} private",
                        "repos", p.public_repos, p.total_private_repos
                    ));
                    lines.push(format!(
                        "{:10} {} \u{b7} following {}",
                        "followers", p.followers, p.following
                    ));
                }
            },
            AccountRow::Scopes => {
                lines.push(format!("{BOLD}What this token may do{OFF}"));
                lines.push(crate::paint::rule(cols));
                match self.scopes() {
                    Scopes::Granted(granted) => {
                        for s in granted.list() {
                            lines.push(format!("  {GRN}\u{2713}{OFF} {s}"));
                        }
                    }
                    Scopes::Asking => {
                        lines.push(format!("{DIM}waiting for the first response\u{2026}{OFF}"));
                    }
                    Scopes::Unreported => {
                        lines.push("GitHub answered, and named no scopes.".into());
                        lines.push(String::new());
                        lines.push(format!(
                            "{DIM}A fine-grained or app token has permissions{OFF}"
                        ));
                        lines.push(format!(
                            "{DIM}rather than scopes, and no header lists those.{OFF}"
                        ));
                    }
                    Scopes::Unknown => {
                        lines.push(format!("{YEL}The call that would have said failed.{OFF}"));
                    }
                }
                lines.push(String::new());
                lines.push(format!(
                    "{DIM}Read from the X-OAuth-Scopes header of whatever{OFF}"
                ));
                lines.push(format!(
                    "{DIM}call last ran, so it costs no request of its own.{OFF}"
                ));
                lines.push(String::new());
                lines.push(format!("{GRY}To widen it:{OFF}"));
                lines.push(format!(
                    "  {ORG}gh auth refresh -h github.com -s <scope>{OFF}"
                ));
                lines.push(String::new());
                lines.push(format!(
                    "{DIM}The Keys tab names the exact scope it needs, so{OFF}"
                ));
                lines.push(format!("{DIM}there is nothing to look up.{OFF}"));
            }
            AccountRow::NoApi => {
                lines.push(format!("{BOLD}Not reachable from any token{OFF}"));
                lines.push(crate::paint::rule(cols));
                lines.push(format!("  {YEL}\u{2022}{OFF} personal access tokens"));
                lines.push(format!("  {YEL}\u{2022}{OFF} two-factor settings"));
                lines.push(String::new());
                lines.push("GitHub exposes no API for either, at any scope.".into());
                lines.push(String::new());
                lines.push(format!(
                    "{DIM}Said out loud rather than shown as an empty list,{OFF}"
                ));
                lines.push(format!(
                    "{DIM}because a blank screen reads as \"none\" and would{OFF}"
                ));
                lines.push(format!("{DIM}be wrong.{OFF}"));
                lines.push(String::new());
                lines.push(format!("{GRY}github.com/settings/tokens{OFF}"));
            }
        }
        Pane { image: None, lines }
    }

    fn on_action(&mut self, _action: Action, _sel: usize) -> Flow {
        Flow::Continue
    }

    fn footer(&self) -> String {
        match self.at(self.sel) {
            AccountRow::Scopes => "j/k move".into(),
            _ => "j/k move".into(),
        }
    }
}

// ─── Keys ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
struct SshKey {
    id: u64,
    key: String,
    title: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct GpgKey {
    key_id: String,
    #[serde(default)]
    emails: Vec<GpgEmail>,
    can_sign: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
struct GpgEmail {
    email: String,
}

/// One kind of credential, and what came back when we asked for it.
struct Lane {
    name: &'static str,
    path: &'static str,
    state: LaneState,
}

/// The request behind a lane. A full page is asked for, because the default
/// is thirty and a lane that stopped there would look complete.
fn lane_req(path: &str) -> Req {
    Req::Json {
        path: format!("{path}?per_page={PAGE}"),
    }
}

enum LaneState {
    Waiting,
    /// Rendered rows, already formatted -- each lane's shape differs enough
    /// that keeping the raw JSON around would mean re-deciding per draw.
    Items(Vec<(String, String)>),
    /// The scope story, which is the common case on a fresh token.
    Denied(crate::scopes::Denied),
    Failed(String),
}

pub struct Keys {
    fetch: Arc<Fetcher>,
    lanes: Vec<Lane>,
    open: Vec<bool>,
    epoch_seen: u64,
    sel: usize,
    /// Set when we hand the terminal to `gh auth refresh`, so the next tick
    /// re-asks with the token we now have.
    ///
    /// A flag rather than refreshing before the detach, because the detach is
    /// what CHANGES the token -- a request fired beforehand would race the
    /// thing it is waiting for and come back denied again, which looks exactly
    /// like the authorisation not having worked.
    reauthorised: bool,
}

enum At {
    Lane(usize),
    /// A row under a lane. Only the LANE matters -- the pane shows the whole
    /// lane either way, and every action here (refresh, re-authorise) is about
    /// the lane rather than one key in it.
    Item(usize),
    Nothing,
}

impl Keys {
    fn new(fetch: Arc<Fetcher>) -> Self {
        let lanes = vec![
            Lane {
                name: "\u{1f511} ssh keys",
                path: "user/keys",
                state: LaneState::Waiting,
            },
            Lane {
                name: "\u{1f510} gpg keys",
                path: "user/gpg_keys",
                state: LaneState::Waiting,
            },
            Lane {
                name: "\u{1f4dd} signing keys",
                path: "user/ssh_signing_keys",
                state: LaneState::Waiting,
            },
        ];
        for l in &lanes {
            fetch.request(lane_req(l.path));
        }
        let open = vec![true; lanes.len()];
        Self {
            fetch,
            lanes,
            open,
            epoch_seen: 0,
            sel: 0,
            reauthorised: false,
        }
    }

    fn refresh_all(&self) {
        for lane in &self.lanes {
            self.fetch.refresh(lane_req(lane.path));
        }
    }

    fn sync(&mut self) {
        for lane in self.lanes.iter_mut() {
            let got = self.fetch.get(&lane_req(lane.path));
            lane.state = match got {
                // A reply that does not parse is a failure. Read as an empty
                // list it says "this account has none", with the token's
                // blessing, about keys that were simply not understood.
                Some(Got::Json(v)) => match render_lane(lane.path, &v) {
                    Ok(items) => LaneState::Items(items),
                    Err(e) => LaneState::Failed(e),
                },
                // The reason this screen exists. `GET /user/keys` without the
                // scope answers 404, which as an empty list reads as "you have
                // no SSH keys" on an account with two.
                Some(Got::Denied(d)) => LaneState::Denied(d),
                Some(Got::Failed(e)) => LaneState::Failed(e),
                _ => LaneState::Waiting,
            };
        }
    }

    fn at(&self, sel: usize) -> At {
        let mut row = 0;
        for (l, lane) in self.lanes.iter().enumerate() {
            if sel == row {
                return At::Lane(l);
            }
            row += 1;
            if self.open[l] {
                for _ in 0..lane.state.rows_under() {
                    if sel == row {
                        return At::Item(l);
                    }
                    row += 1;
                }
            }
            row += 1; // spacer
        }
        At::Nothing
    }
}

impl LaneState {
    /// How many rows `rows()` draws under an open lane in this state.
    ///
    /// The one place that says it, because `at` counting differently from
    /// what was drawn reads every row below one off. An EMPTY lane draws a
    /// "none on this account" row, and counting it as zero put the cursor on
    /// the next lane's heading while the keys went to that lane's first child
    /// -- so `enter` to collapse a heading ran `gh auth refresh` instead.
    fn rows_under(&self) -> usize {
        match self {
            LaneState::Items(items) => items.len().max(1) + usize::from(self.cut()),
            _ => 1, // the one explanatory row
        }
    }

    /// A full page came back, so there may be more than is shown. Drawn as a
    /// row of its own; a list that stops without saying so reads as the whole
    /// list.
    fn cut(&self) -> bool {
        matches!(self, LaneState::Items(items) if items.len() >= PAGE)
    }
}

/// Turn one lane's JSON into (label, detail) pairs.
///
/// Per-path rather than a trait, because three shapes do not justify one and
/// the differences are the point: an SSH key is identified by its title, a GPG
/// key by its key id and the emails it covers.
///
/// `Err` when the reply is not the list it should be, which is not the same
/// answer as an empty list and must not be drawn as one.
fn render_lane(path: &str, v: &serde_json::Value) -> Result<Vec<(String, String)>, String> {
    let unreadable = |e: serde_json::Error| format!("unreadable response from {path}: {e}");
    Ok(match path {
        "user/keys" | "user/ssh_signing_keys" => {
            Vec::<SshKey>::deserialize(v)
                .map_err(unreadable)?
                .into_iter()
                .map(|k| {
                    let label = k
                        .title
                        .filter(|t| !t.is_empty())
                        .unwrap_or_else(|| format!("key {}", k.id));
                    // The type plus a short fingerprint-ish tail: enough to tell
                    // two keys apart without putting 400 characters in a column.
                    let kind = k.key.split_whitespace().next().unwrap_or("").to_string();
                    let tail: String = k
                        .key
                        .chars()
                        .rev()
                        .take(8)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    (label, format!("{kind} \u{2026}{tail}"))
                })
                .collect()
        }
        "user/gpg_keys" => Vec::<GpgKey>::deserialize(v)
            .map_err(unreadable)?
            .into_iter()
            .map(|k| {
                let who = k
                    .emails
                    .first()
                    .map(|e| e.email.clone())
                    .unwrap_or_else(|| "no email".into());
                let signs = if k.can_sign.unwrap_or(false) {
                    "can sign"
                } else {
                    "cannot sign"
                };
                (k.key_id, format!("{who} \u{b7} {signs}"))
            })
            .collect(),
        _ => Vec::new(),
    })
}

impl Screen for Keys {
    fn title(&self) -> String {
        "keys".into()
    }

    fn focus(&mut self, sel: usize) {
        self.sel = sel;
    }

    fn tick(&mut self) -> Tick {
        // First tick after the terminal comes back from `gh auth refresh`.
        if self.reauthorised {
            self.reauthorised = false;
            self.refresh_all();
            return Tick::Changed;
        }
        if self.fetch.landed(&mut self.epoch_seen) {
            self.sync();
            return Tick::Changed;
        }
        self.fetch.waiting()
    }

    fn rows(&mut self) -> Vec<Row> {
        let mut rows = Vec::new();
        for (l, lane) in self.lanes.iter().enumerate() {
            let (detail, badge) = match &lane.state {
                LaneState::Waiting => ("asking\u{2026}".to_string(), None),
                LaneState::Items(items) => (
                    match items.len() {
                        0 => "none".to_string(),
                        // A count that might be short is not a count.
                        n if lane.state.cut() => format!("{n}+"),
                        n => n.to_string(),
                    },
                    None,
                ),
                // Badged rather than shown as a count, because the one thing
                // this row must not look like is an answer.
                LaneState::Denied(_) => (String::new(), Some(("scope", idx::YEL))),
                LaneState::Failed(_) => (String::new(), Some(("error", idx::RED))),
            };
            let mut r = Row::leaf(lane.name);
            r.group = true;
            r.expanded = self.open[l];
            let mut r = r.detail(detail);
            if let Some((t, c)) = badge {
                r = r.badged(t, c);
            }
            rows.push(r);
            if self.open[l] {
                match &lane.state {
                    LaneState::Items(items) => {
                        for (label, detail) in items {
                            let mut r = Row::leaf(label);
                            r.depth = 1;
                            rows.push(r.detail(detail));
                        }
                        if items.is_empty() {
                            let mut r = Row::leaf("none on this account");
                            r.depth = 1;
                            rows.push(r);
                        }
                        if lane.state.cut() {
                            let mut r = cut_row("there may be more");
                            r.depth = 1;
                            rows.push(r);
                        }
                    }
                    LaneState::Denied(d) => {
                        let mut r = Row::leaf(format!("needs {}", d.suggest));
                        r.depth = 1;
                        rows.push(r.detail("enter to authorise it"));
                    }
                    LaneState::Failed(_) => {
                        let mut r = Row::leaf("could not read");
                        r.depth = 1;
                        rows.push(r);
                    }
                    LaneState::Waiting => {
                        let mut r = Row::leaf("\u{2026}");
                        r.depth = 1;
                        rows.push(r);
                    }
                }
            }
            rows.push(Row::spacer());
        }
        rows
    }

    fn pane(&mut self, sel: usize, cols: u16, _rows: u16) -> Pane {
        let mut lines = Vec::new();
        let l = match self.at(sel) {
            At::Lane(l) | At::Item(l) => l,
            At::Nothing => return Pane { image: None, lines },
        };
        let lane = &self.lanes[l];
        lines.push(format!("{BOLD}{}{OFF}", lane.name));
        lines.push(format!("{GRY}GET /{}{OFF}", lane.path));
        lines.push(crate::paint::rule(cols));
        match &lane.state {
            LaneState::Waiting => lines.push(format!("{DIM}asking github\u{2026}{OFF}")),
            LaneState::Items(items) if items.is_empty() => {
                lines.push("This account has none.".into());
                lines.push(String::new());
                lines.push(format!(
                    "{DIM}And the token CAN see them, so this is an answer{OFF}"
                ));
                lines.push(format!("{DIM}rather than a refusal.{OFF}"));
            }
            LaneState::Items(items) => {
                for (label, detail) in items {
                    lines.push(format!("{PUR}{label}{OFF}"));
                    lines.push(format!("  {GRY}{detail}{OFF}"));
                }
                if lane.state.cut() {
                    lines.push(String::new());
                    lines.push(format!(
                        "{YEL}Only the first {PAGE} are shown.{OFF} GitHub sent a"
                    ));
                    lines.push("full page, so there may be more.".into());
                }
            }
            LaneState::Denied(d) => {
                lines.push(format!("{YEL}Your token cannot read this.{OFF}"));
                lines.push(String::new());
                lines.push(format!(
                    "{DIM}GitHub answers 404 here rather than 403, so an{OFF}"
                ));
                lines.push(format!(
                    "{DIM}honest-looking \"none\" would be wrong -- hence this{OFF}"
                ));
                lines.push(format!("{DIM}page instead.{OFF}"));
                lines.push(String::new());
                lines.push(format!("{GRY}the endpoint accepts any of:{OFF}"));
                for s in &d.accepted {
                    let mark = if *s == d.suggest { ORG } else { DIM };
                    lines.push(format!("  {mark}{s}{OFF}"));
                }
                lines.push(String::new());
                lines.push(format!("{BOLD}Run this:{OFF}"));
                lines.push(format!("  {ORG}{}{OFF}", d.fix()));
                lines.push(String::new());
                lines.push(format!(
                    "{DIM}The narrowest of the three, because this screen only{OFF}"
                ));
                lines.push(format!(
                    "{DIM}reads. Creating keys will ask for write: when it{OFF}"
                ));
                lines.push(format!("{DIM}exists.{OFF}"));
            }
            LaneState::Failed(e) => {
                lines.push(format!("{RED}{e}{OFF}"));
            }
        }
        Pane { image: None, lines }
    }

    fn on_action(&mut self, action: Action, sel: usize) -> Flow {
        // Enter on the row that says what is missing RUNS the thing that fixes
        // it. `gh auth refresh` is interactive -- it prints a one-time code and
        // waits while you paste it into a browser -- so it needs the real tty,
        // which is exactly what `Flow::Detach` hands it. It is also
        // self-confirming: gh says what it is about to do and waits for a
        // keypress, so there is nothing for this screen to ask first.
        //
        // `enter` alone. `l` is a direction: it reaches the pane, where the
        // command is spelt out, and a key pressed to go and read what a row
        // would do must not be the key that does it.
        match (action, self.at(sel)) {
            (Action::Activate, At::Item(l)) => {
                let LaneState::Denied(d) = &self.lanes[l].state else {
                    return Flow::Continue;
                };
                let scope = d.suggest.clone();
                self.reauthorised = true;
                Flow::Detach(vec![
                    "gh".into(),
                    "auth".into(),
                    "refresh".into(),
                    "-h".into(),
                    "github.com".into(),
                    "-s".into(),
                    scope,
                ])
            }
            (Action::Activate, At::Lane(l)) => {
                self.open[l] = !self.open[l];
                Flow::Dirty
            }
            // `l` opens a closed lane and otherwise moves on. Toggling here
            // made it close an open one, which is `h`'s job, and left the
            // pane beside a heading unreachable.
            (Action::Open, At::Lane(l)) if !self.open[l] => {
                self.open[l] = true;
                Flow::Dirty
            }
            (Action::Close, At::Lane(l)) => {
                self.open[l] = false;
                Flow::Dirty
            }
            (Action::Key('r'), _) => {
                self.refresh_all();
                Flow::Dirty
            }
            _ => Flow::Continue,
        }
    }

    fn footer(&self) -> String {
        match self.at(self.sel) {
            At::Lane(_) => "enter open/close \u{b7} r refresh".into(),
            At::Item(l) if matches!(self.lanes[l].state, LaneState::Denied(_)) => {
                "enter authorises the scope \u{b7} r refresh".into()
            }
            _ => "r refresh".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ssh_key_row_says_its_type_and_enough_to_tell_two_apart() {
        let v: serde_json::Value = serde_json::from_str(
            r#"[{"id":1,"key":"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIabcdefgh","title":"laptop"},
                {"id":2,"key":"ssh-rsa AAAAB3NzaC1yc2EAAAADzzzzzzzz","title":""}]"#,
        )
        .expect("parses");
        let rows = render_lane("user/keys", &v).expect("a key list");
        assert_eq!(rows[0].0, "laptop");
        assert!(rows[0].1.starts_with("ssh-ed25519"));
        assert!(rows[0].1.contains("abcdefgh"), "{:?}", rows[0].1);
        // An untitled key still needs a label, or the row is blank.
        assert_eq!(rows[1].0, "key 2");
    }

    #[test]
    fn a_gpg_row_names_the_email_it_covers() {
        let v: serde_json::Value = serde_json::from_str(
            r#"[{"key_id":"ABC123","can_sign":true,"emails":[{"email":"me@example.com"}]}]"#,
        )
        .expect("parses");
        let rows = render_lane("user/gpg_keys", &v).expect("a key list");
        assert_eq!(rows[0].0, "ABC123");
        assert!(rows[0].1.contains("me@example.com"));
        assert!(rows[0].1.contains("can sign"));
    }

    /// A GPG key with no email attached must not panic or render blank.
    #[test]
    fn a_gpg_key_with_no_email_still_renders() {
        let v: serde_json::Value =
            serde_json::from_str(r#"[{"key_id":"NOEMAIL","emails":[]}]"#).expect("parses");
        let rows = render_lane("user/gpg_keys", &v).expect("a key list");
        assert!(rows[0].1.contains("no email"));
        assert!(rows[0].1.contains("cannot sign"));
    }

    const PATHS: [&str; 3] = ["user/keys", "user/gpg_keys", "user/ssh_signing_keys"];

    fn json(text: &str) -> Got {
        Got::Json(Arc::new(serde_json::from_str(text).expect("parses")))
    }

    fn ssh_keys(n: usize) -> Got {
        let keys: Vec<String> = (0..n)
            .map(|i| format!(r#"{{"id":{i},"key":"ssh-ed25519 AAAA{i:08}","title":"k{i}"}}"#))
            .collect();
        json(&format!("[{}]", keys.join(",")))
    }

    fn denied(scope: &str) -> Got {
        Got::Denied(crate::scopes::Denied {
            accepted: vec![scope.to_string()],
            suggest: scope.to_string(),
        })
    }

    /// The Keys tab over three answers already in hand. Seeded BEFORE the
    /// screen is built, so `Keys::new` finds every lane answered and nothing
    /// is asked of the network.
    fn keys(answers: [Got; 3]) -> Keys {
        let fetch = Arc::new(Fetcher::new(""));
        for (path, got) in PATHS.iter().zip(answers) {
            fetch.seed(lane_req(path), got);
        }
        let mut k = Keys::new(fetch);
        k.sync();
        k
    }

    /// Every index must resolve to the row actually drawn at it: a heading to
    /// its lane, a row under it to that same lane, a spacer to nothing.
    fn walk_agrees(k: &mut Keys) {
        let rows = k.rows();
        let mut lane = None;
        for (i, row) in rows.iter().enumerate() {
            match k.at(i) {
                At::Lane(l) => {
                    assert!(row.group, "row {i} ({}) read as a heading", row.label);
                    assert_eq!(row.label, k.lanes[l].name);
                    lane = Some(l);
                }
                At::Item(l) => {
                    assert!(row.selectable && !row.group, "row {i} read as an item");
                    assert_eq!(Some(l), lane, "row {i} ({}) read off its lane", row.label);
                }
                At::Nothing => assert!(!row.selectable, "row {i} ({}) is nothing", row.label),
            }
        }
        assert!(matches!(k.at(rows.len()), At::Nothing));
    }

    #[test]
    fn the_walk_matches_the_rows_in_every_lane_state() {
        // Keys, an EMPTY lane, and a refusal -- the empty one in the middle,
        // where miscounting it moves everything below.
        let mut k = keys([ssh_keys(2), json("[]"), denied("read:ssh_signing_key")]);
        walk_agrees(&mut k);
        k.open[0] = false;
        walk_agrees(&mut k);
        k.open = vec![false; 3];
        walk_agrees(&mut k);

        // A failure, a lane still waiting, and a full page with its extra row.
        let mut k = keys([Got::Failed("boom".into()), Got::Pending, ssh_keys(PAGE)]);
        walk_agrees(&mut k);
        // Every lane empty: three placeholders and no keys at all.
        let mut k = keys([json("[]"), json("[]"), json("[]")]);
        walk_agrees(&mut k);
    }

    /// The row an empty lane draws has to be counted, or the heading below it
    /// is read as the row below THAT -- and `enter` meant to collapse the
    /// signing keys ran `gh auth refresh` for them instead.
    #[test]
    fn enter_on_a_heading_below_an_empty_lane_collapses_that_heading() {
        let mut k = keys([ssh_keys(1), json("[]"), denied("read:ssh_signing_key")]);
        let rows = k.rows();
        let head = rows
            .iter()
            .position(|r| r.label == k.lanes[2].name)
            .expect("the signing heading");
        assert_eq!(rows[head - 2].label, "none on this account");
        assert!(matches!(k.on_action(Action::Activate, head), Flow::Dirty));
        assert!(!k.open[2], "enter on the heading did not close it");
        assert!(!k.reauthorised);
    }

    /// `l` is a direction. It opens a closed lane; on anything else it moves
    /// on, and in particular it neither closes a lane nor hands the terminal
    /// to `gh auth refresh`.
    #[test]
    fn l_opens_a_closed_lane_and_does_nothing_else() {
        let mut k = keys([ssh_keys(1), json("[]"), denied("read:ssh_signing_key")]);
        let rows = k.rows();
        let fix = rows
            .iter()
            .position(|r| r.label.starts_with("needs "))
            .expect("the refusal row");
        assert!(matches!(k.on_action(Action::Open, fix), Flow::Continue));
        assert!(!k.reauthorised, "l ran the authorisation");

        assert!(matches!(k.on_action(Action::Open, 0), Flow::Continue));
        assert!(k.open[0], "l closed an open lane");
        k.open[0] = false;
        assert!(matches!(k.on_action(Action::Open, 0), Flow::Dirty));
        assert!(k.open[0], "l did not open a closed lane");

        // `enter` is still the key that does the thing.
        let fix = k
            .rows()
            .iter()
            .position(|r| r.label.starts_with("needs "))
            .expect("the refusal row");
        match k.on_action(Action::Activate, fix) {
            Flow::Detach(argv) => assert_eq!(
                argv.join(" "),
                "gh auth refresh -h github.com -s read:ssh_signing_key"
            ),
            _ => panic!("enter on the refusal row did not run the fix"),
        }
        assert!(k.reauthorised);
    }

    /// A reply that is not a key list is a failure. As an empty list it reads
    /// "this account has none" -- an answer nobody gave.
    #[test]
    fn a_reply_that_is_not_a_key_list_is_an_error_not_an_empty_lane() {
        let wrong: serde_json::Value = serde_json::from_str(r#"{"message":"x"}"#).expect("parses");
        assert!(render_lane("user/keys", &wrong).is_err());
        assert!(render_lane("user/gpg_keys", &wrong).is_err());

        let mut k = keys([json(r#"{"message":"x"}"#), json("[]"), json("[]")]);
        assert!(matches!(k.lanes[0].state, LaneState::Failed(_)));
        let rows = k.rows();
        assert_eq!(rows[1].label, "could not read");
        let pane = k.pane(1, 80, 24).lines.join("\n");
        assert!(!pane.contains("has none"), "{pane}");
    }

    /// A full page may not be the whole list, and the lane has to say so: a
    /// row under it, a count that admits it, and a line in the pane.
    #[test]
    fn a_full_page_of_keys_says_it_may_have_been_cut() {
        let mut k = keys([ssh_keys(PAGE), json("[]"), ssh_keys(PAGE - 1)]);
        let rows = k.rows();
        assert_eq!(rows[0].detail.as_deref(), Some("100+"));
        assert_eq!(rows[PAGE + 1].label, "first 100 shown");
        assert!(k.pane(0, 80, 24).lines.join("\n").contains("first 100"));
        // One short of a page is the whole list, and says nothing.
        let whole = rows
            .iter()
            .position(|r| r.label == k.lanes[2].name)
            .expect("the signing heading");
        assert_eq!(rows[whole].detail.as_deref(), Some("99"));
        assert!(!rows[whole + 1..]
            .iter()
            .any(|r| r.label.starts_with("first ")));
        // The request asks for a page of that size, or thirty would come back.
        assert_eq!(
            lane_req("user/keys"),
            Req::Json {
                path: "user/keys?per_page=100".into()
            }
        );
    }

    /// The Account tab over a profile answer already in hand.
    fn account(profile: Got) -> Account {
        let fetch = Arc::new(Fetcher::new(""));
        fetch.seed(Account::profile_req(), profile);
        let mut a = Account::new(fetch);
        a.sync();
        a
    }

    /// With the profile still on its way the tab has to keep looking, or it
    /// sits on "fetching" until a key is pressed.
    #[test]
    fn the_account_tab_stays_awake_while_a_request_is_in_flight() {
        let mut a = account(Got::Pending);
        // Something in flight that cannot reach the network: `gh` rejects the
        // flag. Whether it has landed yet or not, idle is the wrong answer.
        a.fetch.refresh(Req::Cmd {
            args: vec!["--definitely-not-a-gh-flag".into()],
        });
        assert!(!matches!(a.tick(), Tick::Idle));
    }

    /// A fine-grained token sends no scopes header, so an answer in hand with
    /// no scopes learned is the final state and must not read as waiting.
    #[test]
    fn a_token_that_reports_no_scopes_is_not_asked_about_forever() {
        let mut a = account(json(r#"{"login":"octocat"}"#));
        let rows = a.rows();
        assert_eq!(rows[0].detail.as_deref(), Some("octocat"));
        assert_eq!(rows[1].detail.as_deref(), Some("none reported"));
        assert!(!a.pane(1, 80, 24).lines.join("\n").contains("waiting"));

        // Before anything has come back, it IS waiting.
        let mut a = account(Got::Pending);
        assert_eq!(a.rows()[1].detail.as_deref(), Some("asking\u{2026}"));
    }

    /// A profile that failed is not still being fetched.
    #[test]
    fn a_failed_profile_does_not_read_as_fetching() {
        let mut a = account(Got::Failed("gh: Bad credentials (HTTP 401)".into()));
        let rows = a.rows();
        assert_ne!(rows[0].detail.as_deref(), Some("fetching\u{2026}"));
        assert_eq!(rows[0].badges[0].0, "error");
        assert_eq!(rows[1].detail.as_deref(), Some("unknown"));
        assert!(a
            .pane(0, 80, 24)
            .lines
            .join("\n")
            .contains("Bad credentials"));
    }

    /// Only something that can be a login is taken as one: what comes back is
    /// offered to the pull request filters as a person.
    #[test]
    fn the_login_is_what_gh_printed_or_nothing() {
        assert_eq!(as_login("roest1\n").as_deref(), Some("roest1"));
        assert_eq!(as_login("some-one_corp").as_deref(), Some("some-one_corp"));
        assert_eq!(as_login(""), None);
        assert_eq!(as_login("\n"), None);
        assert_eq!(as_login("could not find key \"user\""), None);
    }

    /// A token in the environment outranks the stored account, so the stored
    /// login is then not who the calls run as.
    #[test]
    fn a_token_in_the_environment_means_the_stored_login_is_not_me() {
        let env = |set: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                set.iter()
                    .find(|(name, _)| *name == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert!(token_in_env(env(&[("GH_TOKEN", "t")])));
        assert!(token_in_env(env(&[("GITHUB_TOKEN", "t")])));
        assert!(!token_in_env(env(&[])));
        // Set but empty is how a variable gets "unset" in a wrapper script.
        assert!(!token_in_env(env(&[("GH_TOKEN", "")])));
    }
}
