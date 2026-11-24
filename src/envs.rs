//! Environments: the deployment gates, and the picker that edits them.
//!
//! An environment is where a workflow deploys TO -- staging, production -- and
//! what it carries is a set of gates: a wait timer, a list of people who have
//! to approve, a rule about which branches may deploy at all, plus its own
//! secrets. The list endpoint returns every one of those, so the whole screen
//! is one call and the pane needs no second round trip for anything except the
//! environment's secrets and its recent deployments.
//!
//! ## Why every change sends the WHOLE environment
//!
//! `PUT /repos/{owner}/{repo}/environments/{name}` is "create or update an
//! environment", and the documentation does not say what happens to a
//! parameter you leave out. That is the entire reason this file composes the
//! complete desired state on every change rather than sending the one field
//! that moved: the bash implementation sent `{wait_timer: n}` alone, so IF
//! omission clears, setting a wait timer silently dropped the required
//! reviewers -- and nobody would find out until a deploy that should have
//! waited for an approval did not.
//!
//! Sending everything makes the question moot. Whatever the semantics, the
//! environment afterwards is what the pane showed plus the one change, and the
//! confirm in `Menu` prints the exact command so it can be read before it
//! runs.
//!
//! `gh api`'s field syntax carries all of it without a shell or a temp file:
//! `-F 'reviewers[][type]=User' -F 'reviewers[][id]=123'` builds an array of
//! objects, one per pair, in order; `-F 'reviewers[]'` sends an empty array;
//! `-F key[sub]=value` nests; and `-F x=null` sends a real JSON null. Verified
//! against `gh api --verbose` before any of this was written.
//!
//! ## Why required reviewers get a picker
//!
//! Because the API wants a numeric user id, which nobody knows and no amount
//! of typing produces. The collaborators endpoint hands back logins WITH ids,
//! so a list to tick is both easier than typing a username and the only shape
//! that can work at all. It also matches the semantics: the field is a SET
//! that the PUT replaces wholesale, so editing it as a set -- pre-ticked with
//! whoever is required now -- is the only way to add a reviewer without
//! dropping the others.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tui_core::{nav::Action, Flow, Pane, Row, Screen, Tick};

use crate::gh::{Fetcher, Got, Maybe, Req, IN_FLIGHT};
use crate::menu::{Act, Item, Menu, Writes};
use crate::paint::{idx, BOLD, DIM, GRN, GRY, OFF, ORG, PUR, RED, YEL};
use crate::runs::urlencode;
use crate::secrets::{bad_name, envs_req, list_req, scope_args};
use crate::time;

const LIST_EVERY: Duration = Duration::from_secs(60);

// ─── What the API says about an environment ───────────────────────

#[derive(Debug, Clone, Deserialize)]
struct RawList {
    #[serde(default)]
    total_count: usize,
    #[serde(default)]
    environments: Option<Vec<RawEnv>>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawEnv {
    name: String,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    protection_rules: Vec<RawRule>,
    #[serde(default)]
    deployment_branch_policy: Option<RawPolicy>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawRule {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    wait_timer: Option<u64>,
    #[serde(default)]
    prevent_self_review: Option<bool>,
    #[serde(default)]
    reviewers: Vec<RawSlot>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawSlot {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    reviewer: Option<RawWho>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawWho {
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    login: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    slug: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawPolicy {
    #[serde(default)]
    protected_branches: bool,
    #[serde(default)]
    custom_branch_policies: bool,
}

/// Which branches may deploy here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// No policy at all, which the API spells as a null object.
    Any,
    Protected,
    Selected,
}

/// One person or team that has to approve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reviewer {
    pub team: bool,
    pub id: u64,
    pub name: String,
}

impl Reviewer {
    fn key(&self) -> (bool, u64) {
        (self.team, self.id)
    }

    fn kind(&self) -> &'static str {
        if self.team {
            "Team"
        } else {
            "User"
        }
    }
}

/// An environment, flattened out of the three shapes the API splits it into.
#[derive(Debug, Clone)]
pub struct Env {
    pub name: String,
    created_at: Option<String>,
    updated_at: Option<String>,
    pub wait_timer: u64,
    pub prevent_self_review: bool,
    pub reviewers: Vec<Reviewer>,
    pub policy: Policy,
}

impl Env {
    fn gates(&self) -> usize {
        usize::from(self.wait_timer > 0)
            + usize::from(!self.reviewers.is_empty())
            + usize::from(self.policy != Policy::Any)
    }
}

/// The environments one call returned, and how many GitHub says there are.
struct Listing {
    envs: Vec<Env>,
    total: usize,
}

impl Listing {
    /// More exist than the one page asked for holds.
    fn cut(&self) -> bool {
        self.total > self.envs.len()
    }
}

/// The list, or why it cannot be read as one.
///
/// An answer with no `environments` in it is an ERROR, not an empty list.
/// Every field of the reply is optional to serde, so anything at all used to
/// parse as "no environments" -- and on this screen that reads as a
/// repository with nothing to protect, and to the screens above it as an
/// environment that has been deleted.
fn try_list(v: &serde_json::Value) -> Result<Listing, String> {
    let list = RawList::deserialize(v).map_err(|e| format!("unreadable environment list: {e}"))?;
    let total = list.total_count;
    let raw = list
        .environments
        .ok_or("unreadable environment list: it names no environments")?;
    let envs = raw
        .into_iter()
        .map(|e| {
            let mut wait_timer = 0;
            let mut prevent_self_review = false;
            let mut reviewers = Vec::new();
            for rule in &e.protection_rules {
                match rule.kind.as_str() {
                    "wait_timer" => wait_timer = rule.wait_timer.unwrap_or(0),
                    "required_reviewers" => {
                        prevent_self_review = rule.prevent_self_review.unwrap_or(false);
                        for slot in &rule.reviewers {
                            let team = slot.kind.as_deref() == Some("Team");
                            let Some(who) = &slot.reviewer else { continue };
                            let Some(id) = who.id else { continue };
                            // A user carries `login`; a team carries `slug`
                            // and `name`. Either way there is one string
                            // worth printing.
                            let name = who
                                .login
                                .clone()
                                .or_else(|| who.slug.clone())
                                .or_else(|| who.name.clone())
                                .unwrap_or_else(|| id.to_string());
                            reviewers.push(Reviewer { team, id, name });
                        }
                    }
                    _ => {}
                }
            }
            let policy = match &e.deployment_branch_policy {
                None => Policy::Any,
                Some(p) if p.custom_branch_policies => Policy::Selected,
                Some(p) if p.protected_branches => Policy::Protected,
                Some(_) => Policy::Any,
            };
            Env {
                name: e.name,
                created_at: e.created_at,
                updated_at: e.updated_at,
                wait_timer,
                prevent_self_review,
                reviewers,
                policy,
            }
        })
        .collect();
    Ok(Listing { envs, total })
}

/// Two spellings of one environment. GitHub keeps the case you typed and
/// compares without it.
fn same_name(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// The complete environment, as one `gh api` command line.
///
/// `wait` is a string so a menu item can pass `{}` and let `Act::Prompt` fill
/// in what was typed -- the template and the literal share one builder, which
/// is what keeps the two from drifting apart.
///
/// `had_reviewers` is what the environment requires NOW: an empty `reviewers`
/// means "nobody", and saying so needs the explicit empty array -- but only
/// when there was somebody to remove, since sending `reviewers[]` to an
/// environment that has none is a change asking to be a no-op.
pub fn put_argv(
    repo: &str,
    env: &str,
    wait: &str,
    prevent: bool,
    reviewers: &[Reviewer],
    had_reviewers: bool,
    policy: Policy,
) -> Vec<String> {
    let mut argv = vec![
        "gh".to_string(),
        "api".into(),
        "-X".into(),
        "PUT".into(),
        format!("repos/{repo}/environments/{}", urlencode(env)),
        "--silent".into(),
        "-F".into(),
        format!("wait_timer={wait}"),
    ];
    if reviewers.is_empty() {
        if had_reviewers {
            argv.push("-F".into());
            argv.push("reviewers[]".into());
        }
    } else {
        // Only meaningful alongside a reviewer list, and sending it without
        // one is asking the API to validate a rule that does not exist.
        argv.push("-F".into());
        argv.push(format!("prevent_self_review={prevent}"));
        for r in reviewers {
            argv.push("-F".into());
            argv.push(format!("reviewers[][type]={}", r.kind()));
            argv.push("-F".into());
            argv.push(format!("reviewers[][id]={}", r.id));
        }
    }
    match policy {
        Policy::Any => {
            argv.push("-F".into());
            argv.push("deployment_branch_policy=null".into());
        }
        Policy::Protected | Policy::Selected => {
            let custom = policy == Policy::Selected;
            argv.push("-F".into());
            argv.push(format!(
                "deployment_branch_policy[protected_branches]={}",
                !custom
            ));
            argv.push("-F".into());
            argv.push(format!(
                "deployment_branch_policy[custom_branch_policies]={custom}"
            ));
        }
    }
    argv
}

// ─── Keeping a snapshot honest ────────────────────────────────────
//
// Every PUT carries the whole environment, composed from what some screen
// read earlier. That is only safe while what it read is still true, and three
// screens stack here -- the list, one environment's menu, the reviewer picker
// -- each holding its own copy. A second change made from a copy taken before
// the first one sent the old values back: set the wait timer to 30, then stop
// the author approving from the same menu, and the second command said
// `wait_timer=0`.
//
// So anything that may have written is counted, in a `menu::Writes` the three
// share, and a screen whose copy is older than the count acts on nothing
// until it has read the environment again.

/// Reading the environment list again after a write.
///
/// Two waits, not one. `Fetcher::refresh` does nothing while the same request
/// is already in flight, and one that left BEFORE the write can come back
/// with the environment as it was -- which would then be taken for the new
/// one. So this waits for whatever is in flight to land, asks, and waits for
/// that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reread {
    Idle,
    Settling,
    Asked,
}

/// What a screen does about a re-read this tick.
#[derive(Debug, PartialEq, Eq)]
enum Turn {
    Wait,
    /// Nothing older is in flight: ask now.
    Ask,
    /// What was asked for is in the fetcher.
    Landed,
}

impl Reread {
    fn busy(self) -> bool {
        self != Reread::Idle
    }

    /// Start one if anything has written since `seen`. True when it did.
    fn notice(&mut self, seen: &mut u64, writes: &Writes) -> bool {
        if !writes.since(seen) {
            return false;
        }
        *self = Reread::Settling;
        true
    }

    /// One tick's worth, for a caller that has read `Fetcher::epoch` this
    /// tick -- `pending` is then exactly "something is still in flight".
    fn turn(&mut self, fetch: &Fetcher) -> Turn {
        if *self == Reread::Idle || fetch.pending() {
            return Turn::Wait;
        }
        if *self == Reread::Settling {
            *self = Reread::Asked;
            Turn::Ask
        } else {
            *self = Reread::Idle;
            Turn::Landed
        }
    }
}

/// One environment as the fetcher has it now, or the line that says why
/// there is not one: GitHub listing the repository's environments without
/// it, which is an answer, or not knowing, which is not.
fn read_one(fetch: &Fetcher, repo: &str, name: &str) -> Result<Env, String> {
    let unknown = |why: String| format!("could not read {name} back \u{2014} {why}");
    match fetch.get(&envs_req(repo)) {
        Some(Got::Json(v)) => {
            let list = try_list(&v).map_err(unknown)?;
            let at = list
                .envs
                .iter()
                .position(|e| e.name == name)
                .or_else(|| list.envs.iter().position(|e| same_name(&e.name, name)));
            let (cut, listed, total) = (list.cut(), list.envs.len(), list.total);
            match at.and_then(|i| list.envs.into_iter().nth(i)) {
                Some(env) => Ok(env),
                // Absent from one page of several is not absent.
                None if cut => Err(unknown(format!(
                    "only the first {listed} of {total} environments are listed"
                ))),
                None => Err(format!(
                    "{name} is not one of this repository's environments"
                )),
            }
        }
        Some(Got::Denied(d)) => Err(unknown(d.fix())),
        Some(Got::Failed(e)) => Err(unknown(e)),
        _ => Err(unknown("github has not answered".into())),
    }
}

// ─── The list ─────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
enum At {
    Nothing,
    Env(usize),
    New,
}

#[derive(Debug, Clone, Deserialize)]
struct Deployment {
    #[serde(default)]
    created_at: Option<String>,
    #[serde(rename = "ref", default)]
    reference: Option<String>,
    #[serde(default)]
    creator: Option<RawWho>,
}

pub struct Envs {
    repo: String,
    fetch: Arc<Fetcher>,
    envs: Vec<Env>,
    /// How many GitHub says there are, which can be more than one page.
    total: usize,
    /// The list has been read at least once and the last reading worked.
    /// Until then an empty `envs` is "not known", not "none".
    listed: bool,
    epoch_seen: u64,
    asked: Instant,
    error: Option<String>,
    asking: bool,
    status: String,
    sel: usize,
    /// Shared with every menu and picker opened from here.
    writes: Writes,
    /// The count this list was last re-read at.
    seen: u64,
    reread: Reread,
}

fn deploys_req(repo: &str, env: &str) -> Req {
    Req::Json {
        path: format!(
            "repos/{repo}/deployments?environment={}&per_page=5",
            urlencode(env)
        ),
    }
}

impl Envs {
    pub fn open(repo: &str, fetch: Arc<Fetcher>) -> Self {
        fetch.request(envs_req(repo));
        Self::new(repo, fetch)
    }

    /// The screen with nothing asked for yet.
    fn new(repo: &str, fetch: Arc<Fetcher>) -> Self {
        Self {
            repo: repo.to_string(),
            fetch,
            envs: Vec::new(),
            total: 0,
            listed: false,
            epoch_seen: 0,
            asked: Instant::now(),
            error: None,
            asking: false,
            status: String::new(),
            sel: 0,
            writes: Writes::default(),
            seen: 0,
            reread: Reread::Idle,
        }
    }

    fn sync(&mut self) {
        match self.fetch.get(&envs_req(&self.repo)) {
            Some(Got::Json(v)) => match try_list(&v) {
                Ok(list) => {
                    self.total = list.total;
                    self.envs = list.envs;
                    self.listed = true;
                    self.error = None;
                }
                Err(e) => self.failed(e),
            },
            Some(Got::Denied(d)) => self.failed(d.fix()),
            Some(Got::Failed(e)) => self.failed(e),
            _ => {}
        }
        self.want_detail(self.sel);
    }

    /// The list could not be read, so there is no list.
    ///
    /// The rows from the last good reading used to stay up with the error
    /// nowhere on screen, each with a menu behind it built from values nobody
    /// had been able to confirm. The error row takes their place until a
    /// reading works.
    fn failed(&mut self, why: String) {
        self.envs.clear();
        self.total = 0;
        self.listed = false;
        self.error = Some(why);
    }

    fn cut(&self) -> bool {
        self.total > self.envs.len()
    }

    /// Why nothing opens from the list just now, when a write has happened
    /// that it has not caught up with.
    fn behind(&self) -> Option<String> {
        (self.seen != self.writes.count() || self.reread.busy()).then(|| {
            "re-reading the environments \u{2014} nothing opens until they are back".to_string()
        })
    }

    /// Why a new environment cannot be created just now. Creating is a PUT,
    /// and a PUT to a name that exists is an update, so a name has to be
    /// checked against the whole list -- and there has to be one.
    fn cannot_create(&self) -> Option<String> {
        if let Some(why) = self.behind() {
            return Some(why);
        }
        if let Some(e) = &self.error {
            return Some(format!(
                "could not list environments, so a name cannot be checked \u{2014} {e}"
            ));
        }
        if !self.listed {
            return Some("still asking github which environments exist".into());
        }
        self.cut().then(|| {
            format!(
                "only {} of {} environments are listed, so a name cannot be checked \u{2014} o opens the browser",
                self.envs.len(),
                self.total
            )
        })
    }

    /// The pane's two extra calls, for the row under the cursor and the next
    /// one -- the same one-neighbour rule the run list uses.
    fn want_detail(&self, sel: usize) {
        for i in [sel, sel + 1] {
            if let Some(e) = self.envs.get(i) {
                self.fetch.request(list_req(&self.repo, Some(&e.name)));
                self.fetch.request(deploys_req(&self.repo, &e.name));
            }
        }
    }

    /// Ask again for the list, and for the pane of each environment that
    /// has one.
    ///
    /// Not for every environment. That was two `gh` calls apiece each minute
    /// the tab was on screen, visited or not -- twenty environments, forty-one
    /// processes -- to keep fresh a pane nobody had opened. One the cursor has
    /// never reached has no answer to go stale, and `want_detail` asks for it
    /// when it does.
    fn refresh(&mut self) {
        self.fetch.refresh(envs_req(&self.repo));
        for e in &self.envs {
            for req in [
                list_req(&self.repo, Some(&e.name)),
                deploys_req(&self.repo, &e.name),
            ] {
                if self.fetch.get(&req).is_some() {
                    self.fetch.refresh(req);
                }
            }
        }
        self.asked = Instant::now();
    }

    fn at(&self, sel: usize) -> At {
        if sel < self.envs.len() {
            return At::Env(sel);
        }
        // An empty list still draws ONE row -- the "no environments yet"
        // placeholder -- so the rows below it are where a single environment
        // would have put them. Reading this off `envs.len()` alone put the
        // new-environment row one position too high on exactly the repo where
        // it is the only thing you can do.
        // And a list that is one page of several draws a row saying so.
        let listed = self.envs.len().max(1) + usize::from(self.cut());
        if sel == listed + 1 {
            return At::New;
        }
        At::Nothing
    }

    fn secrets_of(&self, env: &str) -> Maybe<Vec<String>> {
        #[derive(Deserialize)]
        struct Named {
            name: String,
        }
        Maybe::of(self.fetch.get(&list_req(&self.repo, Some(env))), |v| {
            Vec::<Named>::deserialize(v).map(|v| v.into_iter().map(|n| n.name).collect())
        })
    }

    fn deploys_of(&self, env: &str) -> Maybe<Vec<Deployment>> {
        Maybe::of(self.fetch.get(&deploys_req(&self.repo, env)), |v| {
            Vec::deserialize(v)
        })
    }
}

impl Screen for Envs {
    fn title(&self) -> String {
        "environments".into()
    }

    fn focus(&mut self, sel: usize) {
        self.sel = sel;
        self.want_detail(sel);
    }

    fn tick(&mut self) -> Tick {
        // Only the top screen ticks, so a write is first noticed here once
        // whatever made it has been closed.
        let mut changed = self.reread.notice(&mut self.seen, &self.writes);
        if self.fetch.landed(&mut self.epoch_seen) {
            self.sync();
            changed = true;
        }
        match self.reread.turn(&self.fetch) {
            // Everything, not the list alone: a secret set from a menu is in
            // this pane too.
            Turn::Ask => self.refresh(),
            Turn::Landed => {
                self.sync();
                // Whatever the footer was saying about waiting is over.
                self.status.clear();
                changed = true;
            }
            Turn::Wait => {}
        }
        if changed {
            return Tick::Changed;
        }
        if self.reread.busy() || self.fetch.pending() {
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
        for e in &self.envs {
            let detail = e
                .created_at
                .as_deref()
                .and_then(time::epoch)
                .map(|t| format!("created {} ago", time::age((time::now() - t).max(0))))
                .unwrap_or_default();
            let mut r = Row::leaf(format!("\u{1f30d} {}", e.name)).detail(detail);
            // Rightmost first, so these read left to right as timer, people,
            // branches -- loosest gate last.
            if e.policy != Policy::Any {
                r = r.badged("\u{1f33f}", idx::GRY);
            }
            if !e.reviewers.is_empty() {
                r = r.badged(format!("\u{1f464} {}", e.reviewers.len()), idx::GRN);
            }
            if e.wait_timer > 0 {
                r = r.badged(format!("\u{23f1} {}m", e.wait_timer), idx::YEL);
            }
            if e.gates() == 0 {
                r = r.badged("no gates", idx::DIM);
            }
            rows.push(r);
        }
        if self.envs.is_empty() {
            rows.push(
                Row::leaf(match &self.error {
                    Some(e) => format!("could not list environments \u{2014} {e}"),
                    // Asked of the answer, not of the fetcher: anything at
                    // all in flight used to make an empty repository say it
                    // was still asking.
                    None if !self.listed => "asking github\u{2026}".into(),
                    None => "no environments yet".into(),
                })
                .detail(String::new()),
            );
        }
        // One page of a hundred, and nothing else would say the rest exist.
        if self.cut() {
            let mut note = Row::leaf(format!(
                "only the first {} of {} are listed",
                self.envs.len(),
                self.total
            ));
            note.selectable = false;
            rows.push(note);
        }
        rows.push(Row::spacer());
        rows.push(Row::leaf("+ new environment").detail("gh api -X PUT"));
        rows
    }

    fn pane(&mut self, sel: usize, cols: u16, _rows: u16) -> Pane {
        let mut lines = Vec::new();
        match self.at(sel) {
            At::New => {
                lines.push(format!("{BOLD}a new environment{OFF}"));
                lines.push(format!("{GRY}a place to deploy to{OFF}"));
                lines.push(crate::paint::rule(cols));
                lines.push(format!(
                    "{DIM}It arrives with no gates: any branch may deploy{OFF}"
                ));
                lines.push(format!(
                    "{DIM}to it and no approval is needed. Add those after,{OFF}"
                ));
                lines.push(format!("{DIM}from its own menu.{OFF}"));
                lines.push(String::new());
                lines.push(format!(
                    "{DIM}A name already in use is refused: creating is the{OFF}"
                ));
                lines.push(format!(
                    "{DIM}same request as changing, and sent bare it could{OFF}"
                ));
                lines.push(format!("{DIM}clear the gates of the one that exists.{OFF}"));
                lines.push(String::new());
                lines.push(format!("{DIM}Creating one needs admin on the repo.{OFF}"));
            }
            At::Nothing => {
                if let Some(e) = &self.error {
                    lines.push(format!("{RED}{e}{OFF}"));
                } else if !self.listed {
                    lines.push(format!("{DIM}asking github\u{2026}{OFF}"));
                } else if self.envs.is_empty() {
                    lines.push(format!("{GRY}Nothing deploys from here yet.{OFF}"));
                    lines.push(String::new());
                    lines.push(format!(
                        "{DIM}An environment is a named deployment target with{OFF}"
                    ));
                    lines.push(format!(
                        "{DIM}its own secrets, its own approvals and its own{OFF}"
                    ));
                    lines.push(format!("{DIM}rule about which branches may reach it.{OFF}"));
                }
            }
            At::Env(i) => {
                let e = &self.envs[i];
                lines.push(format!("{BOLD}\u{1f30d} {}{OFF}", e.name));
                let stamp = |s: &Option<String>| {
                    s.as_deref()
                        .map(|t| t.chars().take(10).collect::<String>())
                        .unwrap_or_else(|| "?".into())
                };
                lines.push(format!(
                    "{DIM}created {} \u{b7} updated {}{OFF}",
                    stamp(&e.created_at),
                    stamp(&e.updated_at)
                ));
                lines.push(crate::paint::rule(cols));
                lines.push(format!("{GRY}gates{OFF}"));
                if e.gates() == 0 {
                    lines.push(format!(
                        "{DIM}   none \u{2014} any job may deploy here{OFF}"
                    ));
                }
                if e.wait_timer > 0 {
                    lines.push(format!(
                        "   {YEL}\u{23f1}{OFF}  wait {} min before deploying",
                        e.wait_timer
                    ));
                }
                if !e.reviewers.is_empty() {
                    let who: Vec<String> = e
                        .reviewers
                        .iter()
                        .map(|r| {
                            if r.team {
                                format!("{} (team)", r.name)
                            } else {
                                r.name.clone()
                            }
                        })
                        .collect();
                    lines.push(format!(
                        "   {GRN}\u{1f464}{OFF} {}{}",
                        who.join(", "),
                        if e.prevent_self_review {
                            " \u{b7} not the author"
                        } else {
                            ""
                        }
                    ));
                }
                lines.push(match e.policy {
                    Policy::Any => format!("   {DIM}\u{1f33f} any branch may deploy{OFF}"),
                    Policy::Protected => {
                        "   \u{1f33f} only protected branches may deploy".to_string()
                    }
                    Policy::Selected => {
                        "   \u{1f33f} only selected branches and tags may deploy".to_string()
                    }
                });
                lines.push(String::new());
                lines.push(format!("{GRY}secrets{OFF}"));
                match self.secrets_of(&e.name) {
                    Maybe::Waiting => lines.push(format!("{DIM}   asking github\u{2026}{OFF}")),
                    // Listing an environment's secrets needs admin, and an
                    // honest-looking "none" on a repo you only read would be
                    // the same lie `Got::Denied` exists to stop telling.
                    Maybe::Cannot(why) => lines.push(format!("   {DIM}{why}{OFF}")),
                    Maybe::Got(s) if s.is_empty() => lines.push(format!("{DIM}   none{OFF}")),
                    Maybe::Got(s) => {
                        for name in s {
                            lines.push(format!("   \u{1f511} {name}"));
                        }
                    }
                }
                lines.push(String::new());
                lines.push(format!("{GRY}recent deployments{OFF}"));
                match self.deploys_of(&e.name) {
                    Maybe::Waiting => lines.push(format!("{DIM}   asking github\u{2026}{OFF}")),
                    Maybe::Cannot(why) => lines.push(format!("   {DIM}{why}{OFF}")),
                    Maybe::Got(d) if d.is_empty() => lines.push(format!("{DIM}   none{OFF}")),
                    Maybe::Got(d) => {
                        for dep in d {
                            let when = dep
                                .created_at
                                .as_deref()
                                .map(|t| t.chars().take(16).collect::<String>().replace('T', " "))
                                .unwrap_or_else(|| "?".into());
                            let who = dep
                                .creator
                                .as_ref()
                                .and_then(|c| c.login.clone())
                                .unwrap_or_else(|| "?".into());
                            lines.push(format!(
                                "   {when} {DIM}{} \u{b7} {who}{OFF}",
                                dep.reference.unwrap_or_default()
                            ));
                        }
                    }
                }
                lines.push(String::new());
                lines.push(format!(
                    "{ORG}enter{OFF} {DIM}for its gates and secrets{OFF}"
                ));
            }
        }
        Pane { image: None, lines }
    }

    fn on_action(&mut self, action: Action, sel: usize) -> Flow {
        self.status.clear();
        match (action, self.at(sel)) {
            (Action::Activate, At::Env(i)) => {
                if let Some(why) = self.behind() {
                    self.status = why;
                    return Flow::Dirty;
                }
                Flow::Push(Box::new(EnvMenu::of(
                    &self.repo,
                    &self.envs[i],
                    Arc::clone(&self.fetch),
                    self.writes.clone(),
                )))
            }
            (Action::Activate, At::New) => {
                // Before the field rather than after it: a name typed and
                // then turned away for a reason known all along is a wasted
                // question.
                if let Some(why) = self.cannot_create() {
                    self.status = why;
                    return Flow::Dirty;
                }
                self.asking = true;
                Flow::Ask {
                    label: "name of the new environment".into(),
                    initial: String::new(),
                }
            }
            (Action::Key('o'), _) => {
                self.status = crate::browse(&format!(
                    "https://github.com/{}/settings/environments",
                    self.repo
                ));
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
        if !std::mem::take(&mut self.asking) {
            return Flow::Continue;
        }
        let Some(name) = text.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) else {
            return Flow::Dirty;
        };
        // Asked again, because the list can have moved while the name was
        // being typed.
        if let Some(why) = self.cannot_create() {
            self.status = why;
            return Flow::Dirty;
        }
        // GitHub compares names without case, so `Production` IS
        // `production`, and the PUT that creates one updates the other. Sent
        // with no fields, as a create is, that may clear its reviewers, its
        // wait timer and its branch policy -- so a name in use never gets as
        // far as a command.
        if let Some(e) = self.envs.iter().find(|e| same_name(&e.name, &name)) {
            self.status = format!(
                "{} already exists \u{2014} enter on its row changes it",
                e.name
            );
            return Flow::Dirty;
        }
        Flow::Push(Box::new(EnvMenu::create(
            &self.repo,
            &name,
            Arc::clone(&self.fetch),
            self.writes.clone(),
        )))
    }

    fn footer(&self) -> String {
        if !self.status.is_empty() {
            return self.status.clone();
        }
        match self.at(self.sel) {
            At::Env(_) => "enter its menu \u{b7} o browser \u{b7} r refresh".into(),
            At::New => "enter names it \u{b7} needs admin on the repo".into(),
            At::Nothing => "o browser \u{b7} r refresh".into(),
        }
    }
}

// ─── One environment's menu ───────────────────────────────────────

fn put(repo: &str, e: &Env, wait: &str, prevent: bool, reviewers: &[Reviewer]) -> Vec<String> {
    put_argv(
        repo,
        &e.name,
        wait,
        prevent,
        reviewers,
        !e.reviewers.is_empty(),
        e.policy,
    )
}

/// Everything that can be done to `e`, and which row asks for a secret's
/// name. Every command in it is composed from `e`, so the menu is only as
/// current as `e` is -- see `EnvMenu`.
fn env_menu(repo: &str, e: &Env, fetch: &Arc<Fetcher>, writes: &Writes) -> (Menu, usize) {
    let mut items = vec![Item::new(
        "Set wait timer",
        "gh api -X PUT",
        Act::Prompt {
            label: "minutes to wait before deploying (0 disables)".into(),
            initial: e.wait_timer.to_string(),
            argv: put(repo, e, "{}", e.prevent_self_review, &e.reviewers),
        },
    )
    .confirm()
    .note(&[
        "Every job targeting this environment waits that long",
        "before it starts, and anyone can cancel it meanwhile.",
        "The whole environment is sent, so its reviewers and",
        "branch policy are unchanged.",
    ])];
    // Only means anything alongside a reviewer list.
    if !e.reviewers.is_empty() {
        let on = !e.prevent_self_review;
        items.push(
            Item::new(
                if on {
                    "Stop the author approving their own deploy"
                } else {
                    "Let the author approve their own deploy"
                },
                "gh api -X PUT prevent_self_review",
                Act::Run(put(repo, e, &e.wait_timer.to_string(), on, &e.reviewers)),
            )
            .confirm(),
        );
    }
    items.push(Item::new(
        "Required reviewers",
        format!(
            "{} now",
            match e.reviewers.len() {
                0 => "nobody".to_string(),
                n => n.to_string(),
            }
        ),
        Act::open({
            let (repo, env) = (repo.to_string(), e.clone());
            let (fetch, writes) = (Arc::clone(fetch), writes.clone());
            move || {
                Box::new(Reviewers::open(
                    &repo,
                    &env,
                    Arc::clone(&fetch),
                    writes.clone(),
                ))
            }
        }),
    ));
    let secret_row = items.len();
    // Not a `confirm()`: gh stops and asks for the value, which is both the
    // prompt and the way out.
    items.push(Item::new(
        "Set a secret here",
        "gh secret set --env",
        Act::Prompt {
            label: format!("name of the new {} secret", e.name),
            initial: String::new(),
            argv: ["gh", "secret", "set", "{}"]
                .into_iter()
                .map(String::from)
                .chain(scope_args(repo, Some(&e.name)))
                .collect(),
        },
    ));
    items.push(
        Item::new(
            "Delete environment",
            "gh api -X DELETE",
            Act::Run(vec![
                "gh".into(),
                "api".into(),
                "-X".into(),
                "DELETE".into(),
                format!("repos/{repo}/environments/{}", urlencode(&e.name)),
                "--silent".into(),
            ]),
        )
        .danger()
        .note(&[
            "Takes every secret in it with it, and any workflow",
            "naming this environment fails until it exists again.",
        ]),
    );
    (Menu::new("environment", e.name.clone(), items), secret_row)
}

/// The one thing to do about an environment that does not exist yet.
fn create_menu(repo: &str, name: &str) -> Menu {
    // A bare PUT with no fields creates it with no gates at all, which is
    // what the GitHub UI's "New environment" does too.
    let item = Item::new(
        "Create it",
        "gh api -X PUT",
        Act::Run(vec![
            "gh".into(),
            "api".into(),
            "-X".into(),
            "PUT".into(),
            format!("repos/{repo}/environments/{}", urlencode(name)),
            "--silent".into(),
        ]),
    )
    .confirm()
    .note(&[
        "It arrives with no gates: any branch may deploy to it",
        "and no approval is needed. Once it exists this becomes",
        "its menu, which is where those are added.",
        "",
        "Needs admin on the repository.",
    ]);
    Menu::new("new environment", name, vec![item])
}

/// One environment's `Menu`, rebuilt from GitHub after anything it ran.
///
/// A plain `Menu` stays open after an item runs, with every other item still
/// holding the command it was built with. Here those commands are the whole
/// environment, so the second one chosen undid the first, and the menu of an
/// environment just deleted would create it again. This wraps the menu: once
/// something has run -- from it, or from the picker it opened -- nothing in it
/// acts until the environment has been read back and the items rebuilt from
/// what came back. If it cannot be read back, nothing acts at all.
struct EnvMenu {
    repo: String,
    name: String,
    fetch: Arc<Fetcher>,
    writes: Writes,
    /// The count `menu` was built at.
    seen: u64,
    menu: Menu,
    /// The row that asks for a secret's name; none while this is still the
    /// menu of an environment that does not exist.
    secret_row: Option<usize>,
    /// The question outstanding is that one, so its answer is vetted here.
    naming: bool,
    reread: Reread,
    /// Why nothing here can act: the environment did not come back.
    lost: Option<String>,
    status: String,
    sel: usize,
}

impl EnvMenu {
    fn of(repo: &str, env: &Env, fetch: Arc<Fetcher>, writes: Writes) -> Self {
        let (menu, secret_row) = env_menu(repo, env, &fetch, &writes);
        Self::around(repo, &env.name, fetch, writes, menu, Some(secret_row))
    }

    /// For a name that is not an environment yet. The same screen, because
    /// what follows a create is exactly what follows any other write: read
    /// it back, and offer what can be done to what is there.
    fn create(repo: &str, name: &str, fetch: Arc<Fetcher>, writes: Writes) -> Self {
        let menu = create_menu(repo, name);
        Self::around(repo, name, fetch, writes, menu, None)
    }

    fn around(
        repo: &str,
        name: &str,
        fetch: Arc<Fetcher>,
        writes: Writes,
        menu: Menu,
        secret_row: Option<usize>,
    ) -> Self {
        Self {
            repo: repo.to_string(),
            name: name.to_string(),
            fetch,
            seen: writes.count(),
            writes,
            menu,
            secret_row,
            naming: false,
            reread: Reread::Idle,
            lost: None,
            status: String::new(),
            sel: 0,
        }
    }

    /// Are the items built from the environment as it is now?
    ///
    /// The count is compared here as well as in `tick`, so a key that
    /// arrives before the tick that would notice is still refused.
    fn current(&self) -> bool {
        self.lost.is_none() && !self.reread.busy() && self.seen == self.writes.count()
    }

    /// The re-read has landed: build the menu again from it, or say why not.
    fn land(&mut self) {
        match read_one(&self.fetch, &self.repo, &self.name) {
            Ok(env) => {
                let (menu, secret_row) = env_menu(&self.repo, &env, &self.fetch, &self.writes);
                self.name = env.name;
                self.menu = menu;
                self.secret_row = Some(secret_row);
                self.menu.focus(self.sel);
                self.lost = None;
                self.status.clear();
            }
            // Deleted, never created, or not known. In each a PUT from here
            // could make one, with none of the gates this menu was showing.
            Err(why) => self.lost = Some(why),
        }
    }

    /// Pass on what the menu answered, noting a command on its way out.
    fn passed(&mut self, flow: Flow) -> Flow {
        if matches!(flow, Flow::Detach(_)) {
            self.writes.mark();
        }
        flow
    }
}

// Everything `Menu` answers for itself is passed through to it. It leaves the
// rest of `Screen` at the defaults, and a method it stops leaving there has
// to be passed through here as well, or this wrapper answers for it.
impl Screen for EnvMenu {
    fn title(&self) -> String {
        self.menu.title()
    }

    fn focus(&mut self, sel: usize) {
        self.sel = sel;
        self.status.clear();
        self.menu.focus(sel);
    }

    fn tick(&mut self) -> Tick {
        self.fetch.epoch();
        let mut changed = self.reread.notice(&mut self.seen, &self.writes);
        match self.reread.turn(&self.fetch) {
            Turn::Ask => self.fetch.refresh(envs_req(&self.repo)),
            Turn::Landed => {
                self.land();
                changed = true;
            }
            Turn::Wait => {}
        }
        if changed {
            Tick::Changed
        } else if self.reread.busy() {
            Tick::Wake(IN_FLIGHT)
        } else {
            Tick::Idle
        }
    }

    fn rows(&mut self) -> Vec<Row> {
        // One row in place of the items, and `on_action` treats it as
        // nothing: there is no item under it to run.
        match &self.lost {
            Some(why) => vec![Row::leaf(why.clone())
                .detail(String::new())
                .badged("!", idx::RED)],
            None => self.menu.rows(),
        }
    }

    fn pane(&mut self, sel: usize, cols: u16, rows: u16) -> Pane {
        if self.current() {
            return self.menu.pane(sel, cols, rows);
        }
        let mut lines = vec![
            format!("{BOLD}\u{1f30d} {}{OFF}", self.name),
            crate::paint::rule(cols),
        ];
        match &self.lost {
            Some(why) => {
                lines.push(format!("{RED}{why}{OFF}"));
                lines.push(String::new());
                lines.push(format!(
                    "{DIM}Every command here carries the whole environment,{OFF}"
                ));
                lines.push(format!(
                    "{DIM}so without knowing what it is now there is nothing{OFF}"
                ));
                lines.push(format!("{DIM}safe to send. r asks github again.{OFF}"));
            }
            None => {
                lines.push(format!("{GRY}reading it back from github\u{2026}{OFF}"));
                lines.push(String::new());
                lines.push(format!(
                    "{DIM}Something just ran. The items are rebuilt from{OFF}"
                ));
                lines.push(format!(
                    "{DIM}what the environment is now, and until then none{OFF}"
                ));
                lines.push(format!("{DIM}of them runs.{OFF}"));
            }
        }
        Pane { image: None, lines }
    }

    fn on_action(&mut self, action: Action, sel: usize) -> Flow {
        self.status.clear();
        if !self.current() {
            return match action {
                Action::Key('r') if self.lost.is_some() && !self.reread.busy() => {
                    self.reread = Reread::Settling;
                    self.status = "asking github again\u{2026}".into();
                    Flow::Dirty
                }
                Action::Activate if self.lost.is_none() => {
                    self.status = format!(
                        "reading {} back \u{2014} nothing runs until it is here",
                        self.name
                    );
                    Flow::Dirty
                }
                _ => Flow::Continue,
            };
        }
        let flow = self.menu.on_action(action, sel);
        self.naming = matches!(flow, Flow::Ask { .. }) && Some(sel) == self.secret_row;
        self.passed(flow)
    }

    fn answered(&mut self, text: Option<String>) -> Flow {
        let naming = std::mem::take(&mut self.naming);
        if !self.current() {
            self.menu.answered(None);
            return Flow::Dirty;
        }
        // The same check the Secrets screen makes, and for the same reason:
        // otherwise a name GitHub will refuse is found out after handing over
        // the terminal and typing the value.
        let name = text.as_deref().map(str::trim).filter(|t| !t.is_empty());
        if let Some(why) = name.filter(|_| naming).and_then(bad_name) {
            self.menu.answered(None);
            self.status = why.into();
            return Flow::Dirty;
        }
        let flow = self.menu.answered(text);
        self.passed(flow)
    }

    fn footer(&self) -> String {
        if !self.status.is_empty() {
            return self.status.clone();
        }
        if self.lost.is_some() {
            return "r asks github again \u{b7} nothing here can run".into();
        }
        if !self.current() {
            return "reading it back from github\u{2026}".into();
        }
        self.menu.footer()
    }
}

// ─── The reviewer picker ──────────────────────────────────────────

/// How many collaborators one call asks for. The answer is a bare array
/// with no count beside it, so a full page is the only sign there are more.
const COLLAB_PAGE: usize = 100;

fn collab_req(repo: &str) -> Req {
    Req::Json {
        path: format!("repos/{repo}/collaborators?per_page={COLLAB_PAGE}"),
    }
}

/// Which row of the picker the cursor is on.
#[derive(Debug, PartialEq, Eq)]
enum Pick {
    /// Row 0, which sends the ticked set.
    Apply,
    Person(usize),
    /// The row that stands in for an empty list, the spacer, a note.
    Nothing,
}

#[derive(Debug, Clone, Deserialize)]
struct Collaborator {
    login: String,
    id: u64,
}

/// Who has to approve a deploy to one environment.
///
/// A set, edited as a set: pre-ticked with whoever is required now, because
/// the PUT replaces the list and adding one person by typing a name would
/// drop everyone else.
pub struct Reviewers {
    repo: String,
    env: String,
    wait_timer: u64,
    prevent: bool,
    policy: Policy,
    had: Vec<Reviewer>,
    fetch: Arc<Fetcher>,
    people: Vec<Reviewer>,
    ticked: BTreeSet<(bool, u64)>,
    epoch_seen: u64,
    error: Option<String>,
    /// The collaborators have been read. Until then an empty list is "not
    /// known", not "nobody".
    listed: bool,
    /// A full page came back, so there are probably more than are shown.
    more: bool,
    writes: Writes,
    /// The count the timer, policy and `had` above were read at.
    seen: u64,
    reread: Reread,
    /// Why the set cannot be sent: the environment did not come back.
    lost: Option<String>,
    status: String,
    sel: usize,
}

impl Reviewers {
    fn open(repo: &str, env: &Env, fetch: Arc<Fetcher>, writes: Writes) -> Self {
        fetch.request(collab_req(repo));
        Self::new(repo, env, fetch, writes)
    }

    /// The picker with nothing asked for yet.
    fn new(repo: &str, env: &Env, fetch: Arc<Fetcher>, writes: Writes) -> Self {
        let mut me = Self {
            repo: repo.to_string(),
            env: env.name.clone(),
            wait_timer: env.wait_timer,
            prevent: env.prevent_self_review,
            policy: env.policy,
            had: env.reviewers.clone(),
            fetch,
            // Whoever is required now is a candidate whether or not they are
            // a collaborator -- a team never will be, and an outside reviewer
            // would otherwise vanish from the list that replaces them.
            people: env.reviewers.clone(),
            ticked: env.reviewers.iter().map(|r| r.key()).collect(),
            epoch_seen: 0,
            error: None,
            listed: false,
            more: false,
            seen: writes.count(),
            writes,
            reread: Reread::Idle,
            lost: None,
            status: String::new(),
            sel: 0,
        };
        me.sort();
        me
    }

    fn sort(&mut self) {
        self.people.sort_by_key(|r| (r.team, r.name.to_lowercase()));
    }

    fn sync(&mut self) {
        match self.fetch.get(&collab_req(&self.repo)) {
            Some(Got::Json(v)) => match Vec::<Collaborator>::deserialize(v.as_ref()) {
                Ok(list) => {
                    self.listed = true;
                    self.more = list.len() >= COLLAB_PAGE;
                    for c in list {
                        let r = Reviewer {
                            team: false,
                            id: c.id,
                            name: c.login,
                        };
                        if !self.people.iter().any(|p| p.key() == r.key()) {
                            self.people.push(r);
                        }
                    }
                    self.sort();
                    self.error = None;
                }
                Err(e) => self.error = Some(format!("unreadable collaborator list: {e}")),
            },
            Some(Got::Denied(d)) => self.error = Some(d.fix()),
            Some(Got::Failed(e)) => self.error = Some(e),
            _ => {}
        }
    }

    fn chosen(&self) -> Vec<Reviewer> {
        self.people
            .iter()
            .filter(|p| self.ticked.contains(&p.key()))
            .cloned()
            .collect()
    }

    /// The one item worth confirming, with the whole command in it.
    fn apply_menu(&self) -> Menu {
        let chosen = self.chosen();
        let argv = put_argv(
            &self.repo,
            &self.env,
            &self.wait_timer.to_string(),
            self.prevent,
            &chosen,
            !self.had.is_empty(),
            self.policy,
        );
        let item = if chosen.is_empty() {
            Item::new(
                "Require nobody",
                "gh api -X PUT reviewers[]",
                Act::Run(argv),
            )
            .danger()
            .note(&[
                "Removes the approval gate entirely. Deploys to this",
                "environment stop waiting for anyone.",
            ])
        } else {
            Item::new(
                match chosen.len() {
                    1 => "Require this 1 reviewer".to_string(),
                    n => format!("Require these {n} reviewers"),
                },
                "gh api -X PUT",
                Act::Run(argv),
            )
            .confirm()
            .note(&[
                "Replaces the reviewer list with exactly the ticked",
                "set. The wait timer and branch policy are sent",
                "unchanged, so nothing else about the environment",
                "moves.",
            ])
        };
        Menu::new("required reviewers", self.env.clone(), vec![item])
    }

    fn at(&self, sel: usize) -> Pick {
        match sel.checked_sub(1) {
            None => Pick::Apply,
            Some(i) if i < self.people.len() => Pick::Person(i),
            // With nobody listed, row 1 is the line saying why. It used to
            // read as the apply row, so enter on "asking github..." opened
            // the menu that requires nobody.
            Some(_) => Pick::Nothing,
        }
    }

    /// Why the ticked set cannot be sent just now.
    ///
    /// The command carries the wait timer and the branch policy as well as
    /// the reviewers, and whether to send an empty list at all depends on who
    /// is required NOW. All of that was read before whatever has run since.
    fn behind(&self) -> Option<String> {
        if let Some(why) = &self.lost {
            return Some(why.clone());
        }
        (self.seen != self.writes.count() || self.reread.busy()).then(|| {
            format!(
                "reading {} back \u{2014} the set is sent once it is here",
                self.env
            )
        })
    }

    /// The re-read has landed: take the environment as it is now. The ticks
    /// are left alone -- they are what is WANTED, not what is.
    fn land(&mut self) {
        match read_one(&self.fetch, &self.repo, &self.env) {
            Ok(env) => {
                self.wait_timer = env.wait_timer;
                self.prevent = env.prevent_self_review;
                self.policy = env.policy;
                for r in &env.reviewers {
                    if !self.people.iter().any(|p| p.key() == r.key()) {
                        self.people.push(r.clone());
                    }
                }
                self.had = env.reviewers;
                self.sort();
                self.lost = None;
                self.status.clear();
            }
            // Gone, and sending the set now would create the environment
            // again; or not known, and it might.
            Err(why) => self.lost = Some(why),
        }
    }
}

impl Screen for Reviewers {
    fn title(&self) -> String {
        format!("required reviewers \u{b7} {}", self.env)
    }

    fn focus(&mut self, sel: usize) {
        self.sel = sel;
        self.status.clear();
    }

    fn tick(&mut self) -> Tick {
        // The apply menu may have sent the set. Only the top screen ticks, so
        // this is first noticed once that menu has closed.
        let mut changed = self.reread.notice(&mut self.seen, &self.writes);
        if self.fetch.landed(&mut self.epoch_seen) {
            self.sync();
            changed = true;
        }
        match self.reread.turn(&self.fetch) {
            Turn::Ask => self.fetch.refresh(envs_req(&self.repo)),
            Turn::Landed => {
                self.land();
                changed = true;
            }
            Turn::Wait => {}
        }
        if changed {
            return Tick::Changed;
        }
        if self.reread.busy() || self.fetch.pending() {
            return Tick::Wake(IN_FLIGHT);
        }
        Tick::Idle
    }

    fn rows(&mut self) -> Vec<Row> {
        let n = self.ticked.len();
        let apply = Row::leaf(match n {
            0 => "\u{25a1} require nobody".to_string(),
            1 => "\u{25a3} require 1 reviewer".to_string(),
            n => format!("\u{25a3} require {n} reviewers"),
        });
        let mut rows = vec![if self.lost.is_some() {
            apply.badged("cannot apply", idx::RED)
        } else if self.behind().is_some() {
            apply.badged("reading\u{2026}", idx::DIM)
        } else {
            apply.badged("apply", idx::GRN)
        }];
        if self.people.is_empty() {
            rows.push(
                Row::leaf(match &self.error {
                    Some(e) => format!("could not list collaborators \u{2014} {e}"),
                    None if !self.listed => "asking github\u{2026}".into(),
                    None => "nobody to pick".into(),
                })
                .detail(String::new()),
            );
            return rows;
        }
        for p in &self.people {
            let box_ = if self.ticked.contains(&p.key()) {
                "\u{25a3}"
            } else {
                "\u{25a1}"
            };
            let mut r = Row::leaf(format!("{box_} {}", p.name));
            r.depth = 1;
            let was = self.had.iter().any(|h| h.key() == p.key());
            rows.push(r.detail(match (p.team, was) {
                (true, true) => "team \u{b7} required now",
                (true, false) => "team",
                (false, true) => "required now",
                (false, false) => "collaborator",
            }));
        }
        // Said out loud even when there ARE rows, because the rows are then
        // only whoever is required ALREADY -- a short list that looks like
        // the whole list is the one way this screen could mislead.
        if let Some(e) = &self.error {
            rows.push(Row::spacer());
            let mut note = Row::leaf(format!("could not list collaborators \u{2014} {e}"));
            note.selectable = false;
            rows.push(note);
        } else if self.more {
            // One page, and a collaborator on the next cannot be ticked.
            rows.push(Row::spacer());
            let mut note = Row::leaf(format!(
                "the first {COLLAB_PAGE} collaborators \u{2014} there may be more"
            ));
            note.selectable = false;
            rows.push(note);
        }
        rows
    }

    fn pane(&mut self, sel: usize, cols: u16, _rows: u16) -> Pane {
        let mut lines = vec![format!("{BOLD}{PUR}{}{OFF}", self.env)];
        match self.at(sel) {
            Pick::Nothing => {
                lines.push(format!("{GRY}collaborators on this repo{OFF}"));
                lines.push(crate::paint::rule(cols));
                match &self.error {
                    Some(e) => lines.push(format!("{RED}{e}{OFF}")),
                    None if !self.listed => {
                        lines.push(format!("{DIM}asking github\u{2026}{OFF}"));
                    }
                    None => lines.push(format!(
                        "{DIM}Nobody is listed, so there is nobody to require.{OFF}"
                    )),
                }
            }
            Pick::Apply => {
                lines.push(format!("{GRY}who has to approve a deploy{OFF}"));
                lines.push(crate::paint::rule(cols));
                lines.push(format!(
                    "{DIM}Only ONE of them has to approve for a job to go{OFF}"
                ));
                lines.push(format!("{DIM}ahead. Up to six, people or teams.{OFF}"));
                lines.push(String::new());
                lines.push(format!(
                    "{DIM}enter here sends the ticked set. It replaces the{OFF}"
                ));
                lines.push(format!(
                    "{DIM}whole list, which is why it starts ticked{OFF}"
                ));
                lines.push(format!("{DIM}with whoever is required now.{OFF}"));
                if let Some(why) = self.behind() {
                    lines.push(String::new());
                    lines.push(format!("{RED}{why}{OFF}"));
                }
                if let Some(e) = &self.error {
                    lines.push(String::new());
                    lines.push(format!("{RED}{e}{OFF}"));
                    lines.push(format!(
                        "{DIM}so the only names here are the ones already{OFF}"
                    ));
                    lines.push(format!(
                        "{DIM}required. Listing collaborators needs push.{OFF}"
                    ));
                }
            }
            Pick::Person(i) => {
                let p = &self.people[i];
                lines.push(format!(
                    "{GRY}{}{OFF}",
                    if p.team {
                        "a team in this org"
                    } else {
                        "a collaborator on this repo"
                    }
                ));
                lines.push(crate::paint::rule(cols));
                lines.push(format!("{:8} {}", "name", p.name));
                lines.push(format!("{:8} {}", "id", p.id));
                lines.push(String::new());
                lines.push(format!(
                    "{DIM}The API takes the id, not the name, which is why{OFF}"
                ));
                lines.push(format!("{DIM}this is a list rather than a field.{OFF}"));
            }
        }
        Pane { image: None, lines }
    }

    fn on_action(&mut self, action: Action, sel: usize) -> Flow {
        self.status.clear();
        match (action, self.at(sel)) {
            // enter on a person ticks them, because there is nothing else it
            // could usefully mean here -- there is no menu behind a name.
            (Action::Activate | Action::Key(' '), Pick::Person(i)) => {
                let key = self.people[i].key();
                if !self.ticked.remove(&key) {
                    self.ticked.insert(key);
                }
                Flow::Dirty
            }
            (Action::Activate, Pick::Apply) => {
                if let Some(why) = self.behind() {
                    self.status = why;
                    return Flow::Dirty;
                }
                // Counted when the menu OPENS, since nothing says afterwards
                // whether its one item ran. Counting a write that did not
                // happen costs a re-read; missing one that did leaves this
                // picker, the menu under it and the list under that all
                // composing their next command from before it.
                self.writes.mark();
                Flow::Push(Box::new(self.apply_menu()))
            }
            (Action::Key('r'), _) => {
                self.fetch.refresh(collab_req(&self.repo));
                if self.lost.is_some() && !self.reread.busy() {
                    self.reread = Reread::Settling;
                }
                Flow::Dirty
            }
            _ => Flow::Continue,
        }
    }

    fn footer(&self) -> String {
        if !self.status.is_empty() {
            return self.status.clone();
        }
        match self.at(self.sel) {
            Pick::Person(_) => "space or enter ticks \u{b7} k to the apply row".into(),
            Pick::Apply if self.lost.is_some() => {
                "r asks github again \u{b7} the set cannot be sent".into()
            }
            Pick::Apply => "enter sends the ticked set \u{b7} j into the list".into(),
            Pick::Nothing => "r asks again \u{b7} k to the apply row".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menu::check;

    const LIVE: &str = r#"{"total_count":2,"environments":[
      {"name":"production","created_at":"2026-01-02T00:00:00Z","updated_at":"2026-02-03T00:00:00Z",
       "protection_rules":[
         {"id":1,"type":"wait_timer","wait_timer":30},
         {"id":2,"type":"required_reviewers","prevent_self_review":true,
          "reviewers":[{"type":"User","reviewer":{"id":7,"login":"roest1"}},
                       {"type":"Team","reviewer":{"id":9,"slug":"platform","name":"Platform"}}]},
         {"id":3,"type":"branch_policy"}],
       "deployment_branch_policy":{"protected_branches":true,"custom_branch_policies":false}},
      {"name":"staging","created_at":"2026-01-02T00:00:00Z",
       "protection_rules":[],"deployment_branch_policy":null}]}"#;

    const REPO: &str = "roest1/dotfiles";

    fn envs() -> Vec<Env> {
        try_list(&serde_json::from_str(LIVE).expect("parses"))
            .expect("reads")
            .envs
    }

    fn json(text: &str) -> Got {
        Got::Json(Arc::new(serde_json::from_str(text).expect("parses")))
    }

    /// A fetcher that has been asked for nothing, so an answer put in by hand
    /// is not raced by the real one. The collaborators are answered already,
    /// because every environment's menu builds its picker ahead of time and
    /// the picker asks for them.
    fn fetcher() -> Arc<Fetcher> {
        let fetch = Arc::new(Fetcher::new(""));
        fetch.seed(collab_req(REPO), json("[]"));
        fetch
    }

    /// The list screen over `LIVE`, as if it had just been read.
    fn listed() -> Envs {
        let mut screen = Envs::new(REPO, fetcher());
        screen.fetch.seed(envs_req(REPO), json(LIVE));
        // The pane's own calls, answered so that nothing here asks GitHub.
        for e in ["production", "staging"] {
            screen.fetch.seed(list_req(REPO, Some(e)), json("[]"));
            screen.fetch.seed(deploys_req(REPO, e), json("[]"));
        }
        screen.sync();
        screen
    }

    /// The minute's refresh re-asks what the pane has been shown and leaves
    /// the rest alone: an environment the cursor never reached is asked for
    /// when it gets there, not every minute until then.
    #[test]
    fn a_refresh_asks_only_for_the_environments_that_have_been_looked_at() {
        let mut screen = Envs::new(REPO, fetcher());
        screen.fetch.seed(
            envs_req(REPO),
            json(
                r#"{"total_count":3,"environments":[
                  {"name":"canary"},{"name":"production"},{"name":"staging"}]}"#,
            ),
        );
        // The cursor opens on the first row, which asks for it and the next.
        screen.sync();
        let calls = |e: &str| [list_req(REPO, Some(e)), deploys_req(REPO, e)];
        for req in calls("canary") {
            screen.fetch.seed(req, json("[]"));
        }
        // Landed, as far as the fetcher knows.
        screen.fetch.settle();

        screen.refresh();
        assert!(screen.fetch.flying(&envs_req(REPO)), "the list itself");
        for req in calls("canary") {
            assert!(screen.fetch.flying(&req), "a pane on show went stale");
        }
        for req in calls("staging") {
            assert!(
                screen.fetch.get(&req).is_none(),
                "asked for an environment nobody has looked at"
            );
        }

        // Reaching it is what asks.
        screen.focus(2);
        for req in calls("staging") {
            assert!(screen.fetch.flying(&req), "not asked for on arrival");
        }
    }

    /// `production`'s menu, on a fetcher holding `LIVE`.
    fn menu_of(name: &str) -> EnvMenu {
        let fetch = fetcher();
        fetch.seed(envs_req(REPO), json(LIVE));
        let env = envs().into_iter().find(|e| e.name == name).expect("listed");
        EnvMenu::of(REPO, &env, fetch, Writes::default())
    }

    fn picker(env: &Env) -> Reviewers {
        Reviewers::new(REPO, env, fetcher(), Writes::default())
    }

    fn labels(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|r| r.label.as_str()).collect()
    }

    /// What the ticks after a write do, with the request itself left out:
    /// nothing is in flight here, so the first turn asks and the second has
    /// "landed" on whatever the test put in the fetcher. The caller's `land`
    /// comes next.
    fn read_back(reread: &mut Reread, seen: &mut u64, writes: &Writes, fetch: &Fetcher) {
        fetch.epoch();
        assert!(reread.notice(seen, writes), "no write was noticed");
        assert_eq!(reread.turn(fetch), Turn::Ask);
        assert_eq!(reread.turn(fetch), Turn::Landed);
        assert!(!reread.busy());
    }

    /// `LIVE` with one thing different about production.
    fn live_with(from: &str, to: &str) -> Got {
        assert!(LIVE.contains(from), "{from} is not in LIVE");
        json(&LIVE.replacen(from, to, 1))
    }

    /// The API splits an environment across three rule objects of different
    /// shapes; this is the one place that has to put it back together.
    #[test]
    fn the_rules_flatten_into_one_environment() {
        let e = envs();
        assert_eq!(e[0].name, "production");
        assert_eq!(e[0].wait_timer, 30);
        assert!(e[0].prevent_self_review);
        assert_eq!(e[0].policy, Policy::Protected);
        assert_eq!(e[0].reviewers.len(), 2);
        assert_eq!(
            e[0].reviewers[0],
            Reviewer {
                team: false,
                id: 7,
                name: "roest1".into()
            }
        );
        // A team carries `slug`, not `login`, and must still be named.
        assert_eq!(
            e[0].reviewers[1],
            Reviewer {
                team: true,
                id: 9,
                name: "platform".into()
            }
        );
        assert_eq!(e[1].wait_timer, 0);
        assert!(e[1].reviewers.is_empty());
        assert_eq!(e[1].policy, Policy::Any);
        assert_eq!(e[1].gates(), 0);
    }

    /// The bug this file exists to not have: a wait timer that drops the
    /// reviewers. Every PUT carries the whole environment.
    #[test]
    fn setting_a_wait_timer_still_sends_the_reviewers() {
        let e = &envs()[0];
        let argv = put_argv(
            "roest1/dotfiles",
            &e.name,
            "5",
            e.prevent_self_review,
            &e.reviewers,
            !e.reviewers.is_empty(),
            e.policy,
        );
        let line = argv.join(" ");
        assert!(line.contains("wait_timer=5"), "{line}");
        assert!(line.contains("prevent_self_review=true"), "{line}");
        assert!(line.contains("reviewers[][type]=User"), "{line}");
        assert!(line.contains("reviewers[][id]=7"), "{line}");
        assert!(line.contains("reviewers[][type]=Team"), "{line}");
        assert!(line.contains("reviewers[][id]=9"), "{line}");
        assert!(
            line.contains("deployment_branch_policy[protected_branches]=true"),
            "{line}"
        );
        // Each reviewer is its own pair, in order, which is what makes gh
        // build an array of objects rather than one merged object.
        let types: Vec<&String> = argv
            .iter()
            .filter(|a| a.starts_with("reviewers[]"))
            .collect();
        assert_eq!(types.len(), 4, "{argv:?}");
    }

    /// Clearing the list needs the explicit empty array -- but only when
    /// there was somebody to clear.
    #[test]
    fn requiring_nobody_sends_an_empty_array_only_when_it_has_to() {
        let had = put_argv("r/r", "prod", "0", false, &[], true, Policy::Any);
        assert!(had.iter().any(|a| a == "reviewers[]"), "{had:?}");
        // prevent_self_review is meaningless with no reviewers, and sending it
        // asks the API to validate a rule that is not there.
        assert!(
            !had.iter().any(|a| a.starts_with("prevent_self_review")),
            "{had:?}"
        );
        let never = put_argv("r/r", "prod", "0", false, &[], false, Policy::Any);
        assert!(!never.iter().any(|a| a == "reviewers[]"), "{never:?}");
    }

    /// "Any branch" is a real null, not an absent field -- the docs are
    /// explicit about that one.
    #[test]
    fn an_open_branch_policy_is_a_null() {
        let any = put_argv("r/r", "e", "0", false, &[], false, Policy::Any).join(" ");
        assert!(any.contains("deployment_branch_policy=null"), "{any}");
        let sel = put_argv("r/r", "e", "0", false, &[], false, Policy::Selected).join(" ");
        assert!(
            sel.contains("[custom_branch_policies]=true")
                && sel.contains("[protected_branches]=false"),
            "{sel}"
        );
    }

    /// An environment name with a space is legal on GitHub and must survive
    /// the path.
    #[test]
    fn an_awkward_name_is_encoded_into_the_path() {
        let argv = put_argv("r/r", "blue green", "0", false, &[], false, Policy::Any);
        assert!(
            argv.iter()
                .any(|a| a == "repos/r/r/environments/blue%20green"),
            "{argv:?}"
        );
    }

    /// The wait-timer item is a TEMPLATE: the same builder produces it, so the
    /// command that confirms is the command that runs.
    #[test]
    fn the_wait_timer_item_keeps_the_rest_of_the_environment() {
        let e = &envs()[0];
        let argv = put(REPO, e, "{}", true, &e.reviewers);
        assert!(argv.iter().any(|a| a == "wait_timer={}"), "{argv:?}");
        assert!(argv.iter().any(|a| a == "reviewers[][id]=7"), "{argv:?}");
    }

    /// The positions and the rows are two readings of one list.
    #[test]
    fn the_rows_and_the_positions_agree() {
        let mut screen = listed();
        let rows = screen.rows();
        assert_eq!(screen.at(0), At::Env(0));
        assert_eq!(screen.at(1), At::Env(1));
        assert_eq!(screen.at(2), At::Nothing, "the spacer");
        assert!(!rows[2].selectable);
        assert_eq!(screen.at(3), At::New);
        assert_eq!(rows[3].label, "+ new environment");
    }

    /// And they have to agree on the EMPTY repo too, which is where it
    /// mattered most and where the first version was wrong: the placeholder
    /// row is a row, so `+ new environment` is at 2 rather than at 1. On a
    /// repo with no environments, creating one is the only thing the screen
    /// can do, and `enter` on it did nothing.
    #[test]
    fn the_positions_survive_an_empty_list() {
        let mut screen = Envs::new(REPO, fetcher());
        screen.fetch.seed(
            envs_req(REPO),
            json(r#"{"total_count":0,"environments":[]}"#),
        );
        screen.sync();
        let rows = screen.rows();
        assert_eq!(
            labels(&rows),
            ["no environments yet", "", "+ new environment"]
        );
        assert_eq!(
            screen.at(0),
            At::Nothing,
            "the placeholder is no environment"
        );
        assert_eq!(screen.at(2), At::New);
        assert!(matches!(
            screen.on_action(Action::Activate, 2),
            Flow::Ask { .. }
        ));
    }

    /// Every other shape the list can take: still asking, failed, and one
    /// page of several. In each the row that is not an environment resolves
    /// to nothing, and `+ new environment` is where `at` says it is.
    #[test]
    fn the_positions_survive_every_shape_that_is_not_a_list() {
        let asking = Envs::new(REPO, fetcher());
        let mut failed = Envs::new(REPO, fetcher());
        failed.fetch.seed(
            envs_req(REPO),
            Got::Failed("gh: Server Error (HTTP 502)".into()),
        );
        failed.sync();
        for (mut screen, says) in [(asking, "asking github"), (failed, "HTTP 502")] {
            let rows = screen.rows();
            assert_eq!(rows.len(), 3, "{:?}", labels(&rows));
            assert!(rows[0].label.contains(says), "{:?}", rows[0].label);
            assert_eq!(screen.at(0), At::Nothing);
            assert!(matches!(
                screen.on_action(Action::Activate, 0),
                Flow::Continue
            ));
            assert!(!rows[1].selectable);
            assert_eq!(rows[2].label, "+ new environment");
            assert_eq!(screen.at(2), At::New);
        }

        let mut cut = listed();
        cut.total = 140;
        let rows = cut.rows();
        assert_eq!(cut.at(0), At::Env(0));
        assert_eq!(cut.at(1), At::Env(1));
        assert!(
            rows[2].label.contains("first 2 of 140"),
            "{:?}",
            rows[2].label
        );
        assert!(!rows[2].selectable && !rows[3].selectable);
        assert_eq!(cut.at(2), At::Nothing);
        assert_eq!(cut.at(3), At::Nothing);
        assert_eq!(rows[4].label, "+ new environment");
        assert_eq!(cut.at(4), At::New);
    }

    /// An environment with no gates says so, rather than looking like one
    /// whose badges have not loaded.
    #[test]
    fn an_ungated_environment_says_no_gates() {
        let mut screen = listed();
        let rows = screen.rows();
        let badges: Vec<&str> = rows[0].badges.iter().map(|b| b.0.as_str()).collect();
        assert!(badges.iter().any(|b| b.contains("30m")), "{badges:?}");
        assert!(badges.iter().any(|b| b.contains('2')), "{badges:?}");
        assert_eq!(
            rows[1].badges.first().map(|b| b.0.as_str()),
            Some("no gates")
        );
    }

    /// The picker opens on the current set, which is the only way to add one
    /// reviewer without removing the others.
    #[test]
    fn the_picker_starts_ticked_with_whoever_is_required_now() {
        let e = &envs()[0];
        let mut p = picker(e);
        assert_eq!(p.ticked.len(), 2);
        // Teams sort after people, so roest1 is row 1 and platform is row 2.
        assert_eq!(p.at(0), Pick::Apply, "row 0 is apply");
        assert_eq!(p.at(1), Pick::Person(0));
        assert_eq!(p.people[0].name, "roest1");
        // Unticking the team leaves the person, and the command says so.
        p.on_action(Action::Key(' '), 2);
        assert_eq!(p.chosen().len(), 1);
        assert_eq!(p.chosen()[0].name, "roest1");
    }

    /// Unticking everybody is a destructive change and has to read as one.
    #[test]
    fn clearing_the_whole_list_is_marked_destructive() {
        let e = &envs()[0];
        let mut p = picker(e);
        p.ticked.clear();
        let mut menu = p.apply_menu();
        assert_eq!(
            menu.rows()[0].badges.first().map(|b| b.0.as_str()),
            Some("!")
        );
        assert!(
            menu.footer().contains("cannot be undone"),
            "{}",
            menu.footer()
        );
    }

    /// A reviewer who is not a collaborator must not disappear from the list
    /// that replaces them.
    #[test]
    fn a_required_reviewer_is_a_candidate_even_if_the_list_never_names_them() {
        let e = &envs()[0];
        let p = picker(e);
        assert_eq!(p.people.len(), 2);
        assert!(p.people.iter().any(|r| r.team && r.name == "platform"));
    }

    /// A payload that is not the shape expected is an error, not a panic and
    /// not an empty list -- a 404 body is JSON too, and "no environments" is
    /// a claim about the repository.
    #[test]
    fn a_wrong_shaped_payload_is_an_error_rather_than_an_empty_list() {
        let v: serde_json::Value = serde_json::from_str(r#"{"message":"Not Found"}"#).unwrap();
        assert!(try_list(&v).is_err());
        let none: serde_json::Value =
            serde_json::from_str(r#"{"total_count":0,"environments":[]}"#).unwrap();
        assert!(try_list(&none)
            .expect("an empty list is a list")
            .envs
            .is_empty());
    }

    // ── A second change, after a first ──

    /// Choose a menu row and say yes to it.
    fn run(m: &mut EnvMenu, row: usize) -> Vec<String> {
        assert!(matches!(m.on_action(Action::Activate, row), Flow::Dirty));
        match m.on_action(Action::Key('y'), row) {
            Flow::Detach(argv) => argv,
            _ => panic!("y did not run row {row}"),
        }
    }

    /// Nothing a key can do to this menu runs, asks or opens anything.
    fn nothing_runs(m: &mut EnvMenu) {
        for row in 0..6 {
            for action in [Action::Activate, Action::Key('y'), Action::Open] {
                let flow = m.on_action(action, row);
                assert!(
                    matches!(flow, Flow::Continue | Flow::Dirty),
                    "row {row} did something on {action:?}"
                );
            }
        }
        assert!(!matches!(m.answered(Some("5".into())), Flow::Detach(_)));
    }

    /// The whole point of the wrapper. Every command in the menu carries the
    /// entire environment as it was when the menu was built, so once one has
    /// run the rest are out of date: the timer went to 5, and the next item
    /// chosen would have put it back to 30.
    #[test]
    fn a_second_change_is_built_from_the_environment_after_the_first() {
        let mut m = menu_of("production");
        assert!(matches!(m.on_action(Action::Activate, 0), Flow::Ask { .. }));
        assert!(matches!(m.answered(Some("5".into())), Flow::Dirty));
        match m.on_action(Action::Key('y'), 0) {
            Flow::Detach(argv) => assert!(argv.iter().any(|a| a == "wait_timer=5"), "{argv:?}"),
            _ => panic!("y did not set the timer"),
        }
        // Still open, and nothing in it acts on what it knew before.
        assert!(!m.current());
        nothing_runs(&mut m);
        assert!(m.footer().contains("reading"), "{}", m.footer());

        m.fetch.seed(
            envs_req(REPO),
            live_with(r#""wait_timer":30"#, r#""wait_timer":5"#),
        );
        read_back(&mut m.reread, &mut m.seen, &m.writes, &m.fetch);
        m.land();
        assert!(m.current());
        let argv = run(&mut m, 1);
        assert!(argv.iter().any(|a| a == "wait_timer=5"), "{argv:?}");
        assert!(
            argv.iter().any(|a| a == "prevent_self_review=false"),
            "{argv:?}"
        );
    }

    /// PUT creates as readily as it updates, so the menu of an environment
    /// that has just been deleted would bring it back with the next item --
    /// minus whatever that item did not happen to carry.
    #[test]
    fn a_deleted_environment_is_not_recreated_from_its_own_menu() {
        let mut m = menu_of("staging");
        let argv = run(&mut m, 3);
        assert_eq!(argv[3], "DELETE", "{argv:?}");
        m.fetch.seed(
            envs_req(REPO),
            json(r#"{"total_count":0,"environments":[]}"#),
        );
        read_back(&mut m.reread, &mut m.seen, &m.writes, &m.fetch);
        m.land();
        let rows = m.rows();
        assert_eq!(rows.len(), 1, "{:?}", labels(&rows));
        assert!(rows[0].label.contains("not one of"), "{:?}", rows[0].label);
        nothing_runs(&mut m);
        assert!(m.pane(0, 60, 40).lines.join("\n").contains("not one of"));
    }

    /// Not being able to read the environment back is not the same as it
    /// being unchanged. Nothing runs, and `r` asks again.
    #[test]
    fn an_environment_that_cannot_be_read_back_runs_nothing() {
        for got in [
            Got::Failed("gh: Server Error (HTTP 502)".into()),
            json(r#"{"message":"hello"}"#),
            // Absent from one page of several is not known to be gone.
            json(r#"{"total_count":140,"environments":[{"name":"production"}]}"#),
        ] {
            let mut m = menu_of("staging");
            run(&mut m, 3);
            m.fetch.seed(envs_req(REPO), got);
            read_back(&mut m.reread, &mut m.seen, &m.writes, &m.fetch);
            m.land();
            assert!(
                m.rows()[0].label.contains("could not read staging back"),
                "{:?}",
                m.rows()[0].label
            );
            nothing_runs(&mut m);
            assert!(matches!(m.on_action(Action::Key('r'), 0), Flow::Dirty));
            assert!(m.reread.busy(), "r did not ask again");
            // And once it does come back, the menu is its menu again.
            m.fetch.seed(envs_req(REPO), json(LIVE));
            m.fetch.epoch();
            assert_eq!(m.reread.turn(&m.fetch), Turn::Ask);
            assert_eq!(m.reread.turn(&m.fetch), Turn::Landed);
            m.land();
            assert!(m.current());
            assert_eq!(m.rows().len(), 4);
        }
    }

    /// The list under the menu holds a copy of its own, and the count is how
    /// it finds out. Until it has read the list again no row opens a menu --
    /// that menu would be built from before the write -- and no name can be
    /// checked against it.
    #[test]
    fn the_list_opens_nothing_until_it_has_caught_up_with_a_write() {
        let mut screen = listed();
        let Flow::Push(mut menu) = screen.on_action(Action::Activate, 0) else {
            panic!("enter on an environment opens its menu");
        };
        assert!(screen.behind().is_none(), "opening a menu is not a write");
        assert!(matches!(menu.on_action(Action::Activate, 1), Flow::Dirty));
        assert!(matches!(
            menu.on_action(Action::Key('y'), 1),
            Flow::Detach(_)
        ));
        for row in [0, 1, 3] {
            assert!(
                matches!(screen.on_action(Action::Activate, row), Flow::Dirty),
                "row {row} opened on a list that predates the write"
            );
            assert!(
                screen.footer().contains("re-reading"),
                "{}",
                screen.footer()
            );
        }
        screen.fetch.seed(
            envs_req(REPO),
            live_with(
                r#""prevent_self_review":true"#,
                r#""prevent_self_review":false"#,
            ),
        );
        read_back(
            &mut screen.reread,
            &mut screen.seen,
            &screen.writes,
            &screen.fetch,
        );
        screen.sync();
        assert!(!screen.envs[0].prevent_self_review);
        assert!(matches!(
            screen.on_action(Action::Activate, 0),
            Flow::Push(_)
        ));
    }

    /// A list that could be read once and cannot be read now is not left on
    /// screen as if it had been: every row of it has a menu behind it.
    #[test]
    fn a_list_that_stops_being_readable_is_taken_down() {
        let mut screen = listed();
        screen.fetch.seed(
            envs_req(REPO),
            Got::Failed("gh: Server Error (HTTP 502)".into()),
        );
        screen.sync();
        let rows = screen.rows();
        assert!(rows[0].label.contains("HTTP 502"), "{:?}", labels(&rows));
        assert_eq!(screen.at(0), At::Nothing);
        assert!(matches!(
            screen.on_action(Action::Activate, 0),
            Flow::Continue
        ));
    }

    /// A re-read asks only once nothing older is in flight, and has landed
    /// only once what it asked for is no longer in flight either.
    #[test]
    fn a_re_read_asks_once_and_then_lands() {
        let fetch = fetcher();
        let writes = Writes::default();
        let (mut reread, mut seen) = (Reread::Idle, 0);
        fetch.epoch();
        assert!(!reread.notice(&mut seen, &writes), "nothing was written");
        assert_eq!(reread.turn(&fetch), Turn::Wait);
        writes.mark();
        read_back(&mut reread, &mut seen, &writes, &fetch);
        assert_eq!(reread.turn(&fetch), Turn::Wait, "it asked twice");
    }

    // ── A new environment ──

    /// Creating is a bare PUT, and a PUT to a name that exists is an update
    /// with every field left out. So a name in use -- in any case, since
    /// GitHub compares them without it -- never becomes a command.
    #[test]
    fn a_name_already_in_use_is_never_sent_as_a_create() {
        for name in ["production", "Production", "  STAGING "] {
            let mut screen = listed();
            assert!(matches!(
                screen.on_action(Action::Activate, 3),
                Flow::Ask { .. }
            ));
            let flow = screen.answered(Some(name.into()));
            assert!(matches!(flow, Flow::Dirty), "{name:?} went ahead");
            assert!(
                screen.footer().contains("already exists"),
                "{name:?}: {}",
                screen.footer()
            );
        }
    }

    /// And a name cannot be checked against a list that is not there, is not
    /// all there, or is from before a write.
    #[test]
    fn nothing_is_created_while_the_list_cannot_vouch_for_the_name() {
        let asking = Envs::new(REPO, fetcher());
        let mut failed = listed();
        failed.failed("gh: Server Error (HTTP 502)".into());
        let mut cut = listed();
        cut.total = 140;
        let behind = listed();
        behind.writes.mark();
        for (mut screen, says) in [
            (asking, "still asking"),
            (failed, "could not list"),
            (cut, "only 2 of 140"),
            (behind, "re-reading"),
        ] {
            let new = screen.rows().len() - 1;
            assert_eq!(screen.at(new), At::New);
            assert!(
                matches!(screen.on_action(Action::Activate, new), Flow::Dirty),
                "{says}: a name was asked for"
            );
            assert!(
                screen.footer().contains(says),
                "{says}: {}",
                screen.footer()
            );
            // Even if the question was already open when it became true.
            screen.asking = true;
            assert!(matches!(
                screen.answered(Some("canary".into())),
                Flow::Dirty
            ));
        }
    }

    /// A name that is free is confirmed with the command it becomes, the
    /// same way everything else here that writes is: enter asks, the pane
    /// shows the line, and only `y` runs it.
    #[test]
    fn a_new_environment_is_confirmed_with_its_command() {
        let mut screen = listed();
        screen.on_action(Action::Activate, 3);
        let Flow::Push(mut menu) = screen.answered(Some(" blue green ".into())) else {
            panic!("a free name opens its confirm");
        };
        assert_eq!(labels(&menu.rows()), ["Create it"]);
        assert!(matches!(menu.on_action(Action::Activate, 0), Flow::Dirty));
        let pane = menu.pane(0, 80, 40).lines.join("\n");
        assert!(pane.contains("About to run"), "{pane}");
        assert!(pane.contains("environments/blue%20green"), "{pane}");
        match menu.on_action(Action::Key('y'), 0) {
            Flow::Detach(argv) => assert_eq!(
                argv,
                [
                    "gh",
                    "api",
                    "-X",
                    "PUT",
                    "repos/roest1/dotfiles/environments/blue%20green",
                    "--silent"
                ]
            ),
            _ => panic!("y creates it"),
        }
        // Sent once. The same row a second time would be the bare PUT to an
        // environment that now exists.
        assert!(!matches!(
            menu.on_action(Action::Activate, 0),
            Flow::Detach(_)
        ));
        assert!(!matches!(
            menu.on_action(Action::Key('y'), 0),
            Flow::Detach(_)
        ));
        assert!(screen.behind().is_some(), "the list did not hear of it");
    }

    /// Once created it is read back like any other write, and what was the
    /// confirm becomes the new environment's own menu. If it is not there --
    /// the create was refused -- nothing is offered at all.
    #[test]
    fn a_created_environment_is_read_back_before_anything_else_is_offered() {
        let make = || {
            let mut m = EnvMenu::create(REPO, "canary", fetcher(), Writes::default());
            run(&mut m, 0);
            m
        };
        let mut m = make();
        m.fetch.seed(
            envs_req(REPO),
            json(r#"{"total_count":1,"environments":[{"name":"canary"}]}"#),
        );
        read_back(&mut m.reread, &mut m.seen, &m.writes, &m.fetch);
        m.land();
        let rows = m.rows();
        assert_eq!(rows[0].label, "Set wait timer", "{:?}", labels(&rows));
        assert!(!labels(&rows).contains(&"Create it"));

        let mut m = make();
        m.fetch.seed(envs_req(REPO), json(LIVE));
        read_back(&mut m.reread, &mut m.seen, &m.writes, &m.fetch);
        m.land();
        assert!(m.rows()[0].label.contains("not one of"));
        nothing_runs(&mut m);
    }

    // ── The menu's own rules ──

    /// A secret named from an environment's menu gets the check the Secrets
    /// screen makes, before gh is handed the terminal -- and only that row's
    /// answer does: thirty minutes is not a secret's name.
    #[test]
    fn a_secret_named_from_the_menu_is_checked_before_gh_is_run() {
        for (name, why) in [
            ("GITHUB_TOKEN", "reserved"),
            ("MY-TOKEN", "underscores only"),
            ("2FA", "starts with"),
        ] {
            let mut m = menu_of("staging");
            assert_eq!(m.secret_row, Some(2));
            assert!(matches!(m.on_action(Action::Activate, 2), Flow::Ask { .. }));
            let flow = m.answered(Some(name.into()));
            assert!(
                !matches!(flow, Flow::Detach(_)),
                "{name:?} was handed to gh"
            );
            assert!(m.footer().contains(why), "{name:?}: {}", m.footer());
            // The question is closed, not left for the next answer to fill.
            assert!(matches!(m.answered(Some("FINE".into())), Flow::Continue));
        }
        let mut m = menu_of("staging");
        m.on_action(Action::Activate, 2);
        match m.answered(Some(" DEPLOY_KEY ".into())) {
            Flow::Detach(argv) => assert_eq!(
                argv,
                [
                    "gh",
                    "secret",
                    "set",
                    "DEPLOY_KEY",
                    "--repo",
                    REPO,
                    "--env",
                    "staging"
                ]
            ),
            _ => panic!("a good name runs gh"),
        }
        let mut m = menu_of("staging");
        m.on_action(Action::Activate, 0);
        assert!(matches!(m.answered(Some("30".into())), Flow::Dirty));
        assert!(m.pane(0, 80, 40).lines.join("\n").contains("wait_timer=30"));
    }

    /// Over every item of every menu this file builds: a command names the
    /// repository, so it does not matter where `github` was started, and one
    /// that changes something asks first. `gh secret set` is the exception by
    /// design -- it stops for the value, which is the prompt and the way out.
    #[test]
    fn every_menu_item_names_the_repo_and_asks_before_it_changes_anything() {
        let all = envs();
        let mut menus: Vec<Menu> = all
            .iter()
            .map(|e| env_menu(REPO, e, &fetcher(), &Writes::default()).0)
            .collect();
        menus.push(create_menu(REPO, "canary"));
        for e in &all {
            let mut p = picker(e);
            menus.push(p.apply_menu());
            p.ticked.clear();
            menus.push(p.apply_menu());
        }
        let mut seen = 0;
        for menu in &menus {
            for item in menu.items() {
                let Some(argv) = check::argv_of(item) else {
                    continue;
                };
                seen += 1;
                assert_eq!(argv[0], "gh", "{:?}: {argv:?}", item.label);
                assert!(
                    check::names_repo(argv, REPO),
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
        assert_eq!(seen, 4 + 3 + 1 + 4, "an item went unchecked");
    }

    // ── The picker ──

    /// With nobody to list there is one row under the apply row saying why,
    /// and it is not a second apply row. Enter on "asking github..." used to
    /// open the menu that requires nobody.
    #[test]
    fn the_line_that_stands_in_for_an_empty_picker_does_nothing() {
        let staging = &envs()[1];
        let asking = picker(staging);
        let mut failed = picker(staging);
        failed.error = Some("gh: Not Found (HTTP 404)".into());
        let mut nobody = picker(staging);
        nobody.sync();
        for (mut p, says) in [
            (asking, "asking github"),
            (failed, "HTTP 404"),
            (nobody, "nobody to pick"),
        ] {
            let rows = p.rows();
            assert_eq!(rows.len(), 2, "{says}: {:?}", labels(&rows));
            assert!(rows[1].label.contains(says), "{:?}", rows[1].label);
            assert_eq!(p.at(0), Pick::Apply);
            assert_eq!(p.at(1), Pick::Nothing);
            for action in [Action::Activate, Action::Key(' '), Action::Open] {
                assert!(
                    matches!(p.on_action(action, 1), Flow::Continue),
                    "{says}: {action:?} acted on the placeholder"
                );
            }
            assert!(p.ticked.is_empty());
        }
    }

    /// And with people listed, every row is what `at` says it is: the notes
    /// under the list are nobody.
    #[test]
    fn the_pickers_rows_and_positions_agree() {
        let mut p = picker(&envs()[0]);
        p.error = Some("gh: Not Found (HTTP 404)".into());
        let mut full = picker(&envs()[0]);
        full.more = true;
        for mut p in [p, full] {
            let rows = p.rows();
            assert_eq!(rows.len(), 5, "{:?}", labels(&rows));
            assert_eq!(p.at(0), Pick::Apply);
            for (i, row) in rows.iter().enumerate().skip(1) {
                match p.at(i) {
                    Pick::Person(n) => assert!(row.label.ends_with(&p.people[n].name)),
                    Pick::Nothing => assert!(!row.selectable, "{:?}", row.label),
                    Pick::Apply => panic!("row {i} is a second apply row"),
                }
            }
            assert_eq!(p.at(rows.len()), Pick::Nothing);
        }
    }

    /// The picker's command carries the timer and the policy too, and
    /// whether it says `reviewers[]` depends on who is required NOW. After
    /// its menu has been open -- and may have sent a set -- all of that is
    /// read again before another set can go.
    #[test]
    fn the_picker_sends_nothing_composed_before_its_last_write() {
        let staging = &envs()[1];
        let mut p = picker(staging);
        let ada = Reviewer {
            team: false,
            id: 41,
            name: "ada".into(),
        };
        p.people.push(ada.clone());
        p.on_action(Action::Key(' '), 1);
        assert!(matches!(p.on_action(Action::Activate, 0), Flow::Push(_)));
        // Back from that menu, and before the environment has been re-read.
        assert!(matches!(p.on_action(Action::Activate, 0), Flow::Dirty));
        assert!(
            p.footer().contains("reading staging back"),
            "{}",
            p.footer()
        );

        // It was sent: staging now requires ada, and waits ten minutes
        // because somebody set that elsewhere meanwhile.
        p.fetch.seed(
            envs_req(REPO),
            json(
                r#"{"total_count":1,"environments":[{"name":"staging","protection_rules":[
                  {"type":"wait_timer","wait_timer":10},
                  {"type":"required_reviewers","reviewers":[{"type":"User","reviewer":{"id":41,"login":"ada"}}]}]}]}"#,
            ),
        );
        read_back(&mut p.reread, &mut p.seen, &p.writes, &p.fetch);
        p.land();
        assert_eq!(p.had, [ada]);
        // Unticking her now has somebody to remove, which only the explicit
        // empty array does. Composed from before, it was left out.
        p.on_action(Action::Key(' '), 1);
        let menu = p.apply_menu();
        let argv = check::argv_of(&menu.items()[0]).expect("a command");
        assert!(argv.iter().any(|a| a == "reviewers[]"), "{argv:?}");
        assert!(argv.iter().any(|a| a == "wait_timer=10"), "{argv:?}");
        assert!(matches!(p.on_action(Action::Activate, 0), Flow::Push(_)));
    }

    /// An environment that is gone, or cannot be read, is not sent a set:
    /// the PUT would create it again.
    #[test]
    fn the_picker_sends_nothing_to_an_environment_it_cannot_read_back() {
        for (got, says) in [
            (json(r#"{"total_count":0,"environments":[]}"#), "not one of"),
            (
                Got::Failed("gh: Server Error (HTTP 502)".into()),
                "HTTP 502",
            ),
        ] {
            let mut p = picker(&envs()[0]);
            assert!(matches!(p.on_action(Action::Activate, 0), Flow::Push(_)));
            p.fetch.seed(envs_req(REPO), got);
            read_back(&mut p.reread, &mut p.seen, &p.writes, &p.fetch);
            p.land();
            assert!(matches!(p.on_action(Action::Activate, 0), Flow::Dirty));
            assert!(p.footer().contains(says), "{}", p.footer());
            assert_eq!(
                p.rows()[0].badges.first().map(|b| b.0.as_str()),
                Some("cannot apply")
            );
            // Ticking is still only ticking.
            assert!(matches!(p.on_action(Action::Key(' '), 1), Flow::Dirty));
        }
    }
}
