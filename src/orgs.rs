//! Organisations: the list, and one org's repos, members and teams.
//!
//! The level that works TODAY. `read:org` -- which this token already has --
//! covers members, teams and repos for every org you belong to, and on this
//! machine that is five orgs including one with twenty-one private repos. No
//! new scope, no new permission conversation.
//!
//! ## Enterprises are not here, and that is a decision
//!
//! There is no Enterprise level. Measured before writing any of this: every org
//! on this account is on a free or team plan, and the token has no
//! `read:enterprise`, so GraphQL refuses `viewer.enterprises` outright. A screen
//! for something that cannot exist is a screen to maintain for nothing. If an
//! enterprise ever appears, it slots in as a level ABOVE this one and `Host`
//! already takes the deeper seeded stack that implies.
//!
//! ## Why the org screen is a host
//!
//! An org has three things worth tabbing between and all three are reachable,
//! which is the bar a level has to clear: a strip with one tab is a title that
//! cost a row. The repo level cleared it the same way, in `root`.
//!
//! ## One page, and it says so
//!
//! Every list here is one request for one page of a hundred. That covers the
//! orgs this was written against, and following `Link` headers for the one
//! that outgrows it is a `gh` module change, not a screen one. What a screen
//! can do is not pretend: a list that comes back a full page long ends in a
//! row saying it may have been cut.

use std::sync::Arc;

use serde::Deserialize;
use tui_core::{nav::Action, Flow, Host, Pane, Row, Screen, Tick};

use crate::gh::{Fetcher, Got, Req, PAGE};
use crate::paint::{idx, BOLD, DIM, GRY, OFF, ORG, PUR, RED};

/// The row that ends a list which may have been cut. A row of its own,
/// because a list that stops without saying so reads as the whole list.
pub(crate) fn cut_row(detail: &str) -> Row {
    Row::leaf(format!("first {PAGE} shown")).detail(detail)
}

#[derive(Debug, Clone, Deserialize)]
pub struct Org {
    pub login: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct OrgDetail {
    login: String,
    name: Option<String>,
    description: Option<String>,
    blog: Option<String>,
    location: Option<String>,
    #[serde(default)]
    public_repos: u64,
    #[serde(default)]
    owned_private_repos: u64,
    plan: Option<Plan>,
}

#[derive(Debug, Clone, Deserialize)]
struct Plan {
    name: String,
    #[serde(default)]
    seats: u64,
    #[serde(default)]
    filled_seats: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct Member {
    login: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Team {
    name: String,
    slug: String,
    description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct OrgRepo {
    name: String,
    description: Option<String>,
    private: bool,
    archived: bool,
    default_branch: Option<String>,
    language: Option<String>,
    pushed_at: Option<String>,
}

// ─── The org list ─────────────────────────────────────────────────

pub struct OrgList {
    fetch: Arc<Fetcher>,
    orgs: Vec<Org>,
    epoch_seen: u64,
    error: Option<String>,
    sel: usize,
}

/// `per_page` here is `PAGE`, spelt out because a `const` cannot be built
/// with `format!`. A test holds the two together.
const ORGS_PATH: &str = "user/orgs?per_page=100";

impl OrgList {
    pub fn new(fetch: Arc<Fetcher>) -> Self {
        fetch.request(Req::Json {
            path: ORGS_PATH.into(),
        });
        Self {
            fetch,
            orgs: Vec::new(),
            epoch_seen: 0,
            error: None,
            sel: 0,
        }
    }

    fn detail_req(login: &str) -> Req {
        Req::Json {
            path: format!("orgs/{login}"),
        }
    }

    /// The org drawn at a row, if an org is what is drawn there.
    ///
    /// Not `orgs.get(sel)`: a refresh that fails keeps the last good list in
    /// `orgs` and draws ONE row saying it could not list them, and row 0 of
    /// that screen is not the first org. Read straight off the vector, `enter`
    /// on the error opened an org nobody could see.
    fn at(&self, sel: usize) -> Option<&Org> {
        if self.error.is_some() {
            return None;
        }
        self.orgs.get(sel)
    }

    /// A full page of orgs came back, so there may be more.
    fn cut(&self) -> bool {
        self.orgs.len() >= PAGE
    }

    fn sync(&mut self) {
        match self.fetch.get(&Req::Json {
            path: ORGS_PATH.into(),
        }) {
            Some(Got::Json(v)) => match Vec::<Org>::deserialize(v.as_ref()) {
                Ok(mut orgs) => {
                    orgs.sort_by_key(|o| o.login.to_lowercase());
                    self.orgs = orgs;
                    self.error = None;
                }
                Err(e) => self.error = Some(format!("unreadable org list: {e}")),
            },
            Some(Got::Denied(d)) => {
                self.error = Some(format!("needs {} \u{2014} {}", d.suggest, d.fix()))
            }
            Some(Got::Failed(e)) => self.error = Some(e),
            _ => {}
        }
        self.want_detail(self.sel);
    }

    fn want_detail(&self, sel: usize) {
        for i in [sel, sel + 1] {
            if let Some(o) = self.orgs.get(i) {
                self.fetch.request(Self::detail_req(&o.login));
            }
        }
    }

    /// One org's detail: `Ok(None)` while it is on its way, `Err` when it is
    /// not coming. Kept apart so a failed request is not drawn as one still
    /// being fetched, which never resolves and never says why.
    fn detail_of(&self, login: &str) -> Result<Option<OrgDetail>, String> {
        match self.fetch.get(&Self::detail_req(login)) {
            Some(Got::Json(v)) => OrgDetail::deserialize(v.as_ref())
                .map(Some)
                .map_err(|e| format!("unreadable org: {e}")),
            Some(Got::Denied(d)) => Err(format!("needs {} \u{2014} {}", d.suggest, d.fix())),
            Some(Got::Failed(e)) => Err(e),
            _ => Ok(None),
        }
    }
}

impl Screen for OrgList {
    fn title(&self) -> String {
        "orgs".into()
    }

    fn focus(&mut self, sel: usize) {
        self.sel = sel;
        self.want_detail(sel);
    }

    fn tick(&mut self) -> Tick {
        if self.fetch.landed(&mut self.epoch_seen) {
            self.sync();
            return Tick::Changed;
        }
        self.fetch.waiting()
    }

    fn rows(&mut self) -> Vec<Row> {
        if let Some(e) = &self.error {
            return vec![Row::leaf("could not list orgs").detail(e.clone())];
        }
        if self.orgs.is_empty() {
            return vec![Row::leaf(if self.fetch.pending() {
                "asking github\u{2026}"
            } else {
                "you are in no organisations"
            })];
        }
        let mut rows: Vec<Row> = self
            .orgs
            .iter()
            .map(|o| {
                let row = Row::leaf(&o.login);
                // The plan is the one fact that changes what an org can DO, so
                // it earns the badge; anything else is in the pane.
                match self.detail_of(&o.login).ok().flatten().and_then(|d| d.plan) {
                    Some(p) if p.name == "free" => row.badged("free", idx::DIM),
                    Some(p) => row.badged(p.name, idx::GRN),
                    None => row,
                }
                .detail(
                    o.description
                        .clone()
                        .unwrap_or_default()
                        .replace(['\n', '\t'], " "),
                )
            })
            .collect();
        if self.cut() {
            rows.push(cut_row("there may be more"));
        }
        rows
    }

    fn pane(&mut self, sel: usize, cols: u16, _rows: u16) -> Pane {
        let mut lines = Vec::new();
        if let Some(e) = &self.error {
            lines.push(format!("{RED}{e}{OFF}"));
            return Pane { image: None, lines };
        }
        let Some(org) = self.at(sel) else {
            if self.cut() && sel == self.orgs.len() {
                lines = cut_pane("organisations");
            }
            return Pane { image: None, lines };
        };
        lines.push(format!("{BOLD}{PUR}{}{OFF}", org.login));
        match self.detail_of(&org.login) {
            Ok(Some(d)) => {
                if let Some(n) = &d.name {
                    if n != &d.login {
                        lines.push(format!("{GRY}{n}{OFF}"));
                    }
                }
                if let Some(desc) = &d.description {
                    lines.push(String::new());
                    lines.push(desc.clone());
                }
                lines.push(String::new());
                lines.push(crate::paint::rule(cols));
                lines.push(format!(
                    "{:10} {} public, {} private",
                    "repos", d.public_repos, d.owned_private_repos
                ));
                if let Some(p) = &d.plan {
                    lines.push(format!(
                        "{:10} {}{}",
                        "plan",
                        p.name,
                        if p.seats > 0 {
                            format!(" \u{b7} {}/{} seats", p.filled_seats, p.seats)
                        } else {
                            String::new()
                        }
                    ));
                }
                for (k, v) in [("where", d.location.clone()), ("web", d.blog.clone())] {
                    if let Some(v) = v.filter(|s| !s.is_empty()) {
                        lines.push(format!("{k:10} {v}"));
                    }
                }
            }
            // The list already carried a description, so that much is shown
            // whether the detail is late or not coming at all.
            short => {
                if let Some(d) = &org.description {
                    lines.push(String::new());
                    lines.push(d.clone());
                }
                lines.push(String::new());
                match short {
                    Err(e) => {
                        lines.push(format!("{RED}could not read its details{OFF}"));
                        lines.push(format!("{RED}{e}{OFF}"));
                    }
                    _ => lines.push(format!("{DIM}fetching details\u{2026}{OFF}")),
                }
            }
        }
        lines.push(String::new());
        lines.push(format!("{DIM}enter for its repos, members and teams{OFF}"));
        Pane { image: None, lines }
    }

    fn on_action(&mut self, action: Action, sel: usize) -> Flow {
        match action {
            Action::Activate => match self.at(sel) {
                Some(o) => Flow::Push(Box::new(org_screen(Arc::clone(&self.fetch), &o.login))),
                None => Flow::Continue,
            },
            Action::Key('r') => {
                self.fetch.refresh(Req::Json {
                    path: ORGS_PATH.into(),
                });
                Flow::Dirty
            }
            _ => Flow::Continue,
        }
    }

    fn footer(&self) -> String {
        match self.at(self.sel) {
            Some(_) => "enter open \u{b7} r refresh".into(),
            None => "r refresh".into(),
        }
    }
}

/// What the pane says on the row that ends a list which may have been cut.
fn cut_pane(what: &str) -> Vec<String> {
    vec![
        format!("{BOLD}The first {PAGE} {what}{OFF}"),
        String::new(),
        "GitHub sent a full page, which is as many as one".into(),
        "request returns. There may be more, and this".into(),
        "list would not show them.".into(),
        String::new(),
        format!("{DIM}github.com has the whole list.{OFF}"),
    ]
}

// ─── One org ──────────────────────────────────────────────────────

/// An org's three tabs. All reachable with `read:org`.
pub fn org_screen(fetch: Arc<Fetcher>, login: &str) -> Host {
    Host::new(
        Some(login.to_string()),
        vec![
            (
                "\u{1f4e6} Repos".into(),
                Box::new(OrgPane::repos(Arc::clone(&fetch), login)) as Box<dyn Screen>,
            ),
            (
                "\u{1f465} Members".into(),
                Box::new(OrgPane::members(Arc::clone(&fetch), login)),
            ),
            (
                "\u{1f3f7} Teams".into(),
                Box::new(OrgPane::teams(fetch, login)),
            ),
        ],
    )
}

/// One row, decided before it becomes a `Row`.
///
/// A named type rather than the tuple it started as: three of these nested two
/// deep is the kind of signature you have to decode every time you read it,
/// and the fields have obvious names.
struct Line {
    label: String,
    detail: String,
    badge: Option<(String, u8)>,
}

impl Line {
    fn plain(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: detail.into(),
            badge: None,
        }
    }
}

/// Which of an org's lists this is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Repos,
    Members,
    Teams,
}

/// One flat list about one org.
///
/// Three kinds in one type rather than three near-identical screens, because
/// the only differences are the path, how a row reads and what the pane says --
/// and all three are a `match` on four arms. Three files would have been three
/// copies of the fetch-and-tick dance.
pub struct OrgPane {
    fetch: Arc<Fetcher>,
    org: String,
    kind: Kind,
    path: String,
    epoch_seen: u64,
    error: Option<String>,
    rows: Vec<Line>,
    /// What the last key did, when the screen would otherwise not show it --
    /// a browser that could not be started looks exactly like a dead key.
    status: String,
    sel: usize,
}

/// The one request behind a list.
fn list_path(org: &str, kind: Kind) -> String {
    match kind {
        Kind::Repos => format!("orgs/{org}/repos?per_page={PAGE}&sort=pushed"),
        Kind::Members => format!("orgs/{org}/members?per_page={PAGE}"),
        Kind::Teams => format!("orgs/{org}/teams?per_page={PAGE}"),
    }
}

impl OrgPane {
    fn make(fetch: Arc<Fetcher>, org: &str, kind: Kind) -> Self {
        let path = list_path(org, kind);
        fetch.request(Req::Json { path: path.clone() });
        Self {
            fetch,
            org: org.to_string(),
            kind,
            path,
            epoch_seen: 0,
            error: None,
            rows: Vec::new(),
            status: String::new(),
            sel: 0,
        }
    }

    fn repos(fetch: Arc<Fetcher>, org: &str) -> Self {
        Self::make(fetch, org, Kind::Repos)
    }
    fn members(fetch: Arc<Fetcher>, org: &str) -> Self {
        Self::make(fetch, org, Kind::Members)
    }
    fn teams(fetch: Arc<Fetcher>, org: &str) -> Self {
        Self::make(fetch, org, Kind::Teams)
    }

    /// A full page came back, so `rows` may not be all of them.
    fn cut(&self) -> bool {
        self.rows.len() >= PAGE
    }

    fn sync(&mut self) {
        let got = self.fetch.get(&Req::Json {
            path: self.path.clone(),
        });
        match got {
            // A reply that does not parse is an error. As an empty list it
            // would be drawn as "nothing here", about an org with repos in it.
            Some(Got::Json(v)) => match build_rows(self.kind, &v) {
                Ok(rows) => {
                    self.rows = rows;
                    self.error = None;
                }
                Err(e) => self.error = Some(e),
            },
            Some(Got::Denied(d)) => self.error = Some(d.fix()),
            Some(Got::Failed(e)) => self.error = Some(e),
            _ => {}
        }
    }

    /// The line drawn at a row, if a line is what is drawn there.
    ///
    /// After a failed refresh `rows` still holds the last good list while the
    /// screen draws a single "could not read" row, and a list that may have
    /// been cut ends in a row that is not one of its items. Neither is
    /// `rows[sel]`, and `o` acting as if it were opened a page for something
    /// that was not on screen.
    fn at(&self, sel: usize) -> Option<&Line> {
        if self.error.is_some() {
            return None;
        }
        self.rows.get(sel)
    }

    /// Where `o` goes from a row, if it goes anywhere.
    fn url_at(&self, sel: usize) -> Option<String> {
        let label = &self.at(sel)?.label;
        Some(match self.kind {
            Kind::Repos => format!("https://github.com/{}/{}", self.org, label),
            Kind::Members => format!("https://github.com/{label}"),
            Kind::Teams => format!("https://github.com/orgs/{}/teams", self.org),
        })
    }

    fn body(&self, sel: usize, cols: u16) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(e) = &self.error {
            lines.push(format!("{RED}{e}{OFF}"));
            return lines;
        }
        let Some(Line { label, detail, .. }) = self.at(sel) else {
            if self.cut() && sel == self.rows.len() {
                return cut_pane(match self.kind {
                    Kind::Repos => "repos, most recently pushed",
                    Kind::Members => "members",
                    Kind::Teams => "teams",
                });
            }
            return lines;
        };
        match self.kind {
            Kind::Repos => {
                lines.push(format!("{BOLD}{}/{}{OFF}", self.org, label));
                lines.push(format!("{GRY}{detail}{OFF}"));
                lines.push(crate::paint::rule(cols));
                lines.push(format!(
                    "{DIM}Not cloned here, or it would be in the Repos tab{OFF}"
                ));
                lines.push(format!(
                    "{DIM}of your account, read from disk. This list is the{OFF}"
                ));
                lines.push(format!("{DIM}org's, from the API.{OFF}"));
                lines.push(String::new());
                lines.push(format!("{ORG}o{OFF} opens it on github.com"));
            }
            Kind::Members => {
                lines.push(format!("{BOLD}{label}{OFF}"));
                lines.push(format!("{GRY}member of {}{OFF}", self.org));
                lines.push(crate::paint::rule(cols));
                lines.push(format!(
                    "{DIM}Roles and team membership need admin:org, which{OFF}"
                ));
                lines.push(format!("{DIM}this token does not have.{OFF}"));
                lines.push(String::new());
                lines.push(format!("{ORG}o{OFF} opens their profile"));
            }
            Kind::Teams => {
                lines.push(format!("{BOLD}{label}{OFF}"));
                lines.push(format!("{GRY}{detail}{OFF}"));
                lines.push(crate::paint::rule(cols));
                lines.push(format!(
                    "{DIM}Team members and repo access need admin:org.{OFF}"
                ));
            }
        }
        lines
    }
}

/// One `Line` per row, per kind.
///
/// `Err` when the reply is not the list this kind expects. That is a
/// different answer from an empty list and has to stay one.
fn build_rows(kind: Kind, v: &serde_json::Value) -> Result<Vec<Line>, String> {
    let unreadable = |what: &str, e: serde_json::Error| format!("unreadable {what} list: {e}");
    Ok(match kind {
        Kind::Repos => Vec::<OrgRepo>::deserialize(v)
            .map_err(|e| unreadable("repo", e))?
            .into_iter()
            .map(|r| {
                // Say the unusual thing: private is the norm in these orgs so
                // PUBLIC is what is worth badging, and archived outranks both
                // because it means "do not touch this".
                let badge = if r.archived {
                    Some(("archived".to_string(), idx::DIM))
                } else if !r.private {
                    Some(("public".to_string(), idx::GRN))
                } else {
                    None
                };
                let mut detail = r.language.unwrap_or_default();
                if let Some(b) = r.default_branch.filter(|b| b != "main" && b != "master") {
                    if !detail.is_empty() {
                        detail.push_str(" \u{b7} ");
                    }
                    detail.push_str(&b);
                }
                if detail.is_empty() {
                    detail = r.description.unwrap_or_default().replace(['\n', '\t'], " ");
                }
                if let Some(when) = r.pushed_at.as_deref().and_then(crate::time::epoch) {
                    let age = crate::time::age((crate::time::now() - when).max(0));
                    detail = if detail.is_empty() {
                        age
                    } else {
                        format!("{age} \u{b7} {detail}")
                    };
                }
                Line {
                    label: r.name,
                    detail,
                    badge,
                }
            })
            .collect(),
        Kind::Members => Vec::<Member>::deserialize(v)
            .map_err(|e| unreadable("member", e))?
            .into_iter()
            .map(|m| Line::plain(m.login, ""))
            .collect(),
        Kind::Teams => Vec::<Team>::deserialize(v)
            .map_err(|e| unreadable("team", e))?
            .into_iter()
            .map(|t| {
                let detail = t.description.unwrap_or_default().replace(['\n', '\t'], " ");
                let detail = if detail.is_empty() { t.slug } else { detail };
                Line::plain(t.name, detail)
            })
            .collect(),
    })
}

impl Screen for OrgPane {
    fn title(&self) -> String {
        self.org.clone()
    }

    fn focus(&mut self, sel: usize) {
        self.sel = sel;
        self.status.clear();
    }

    fn tick(&mut self) -> Tick {
        if self.fetch.landed(&mut self.epoch_seen) {
            self.sync();
            return Tick::Changed;
        }
        self.fetch.waiting()
    }

    fn rows(&mut self) -> Vec<Row> {
        if let Some(e) = &self.error {
            return vec![Row::leaf("could not read").detail(e.clone())];
        }
        if self.rows.is_empty() {
            return vec![Row::leaf(if self.fetch.pending() {
                "asking github\u{2026}"
            } else {
                "nothing here"
            })];
        }
        let mut rows: Vec<Row> = self
            .rows
            .iter()
            .map(|line| {
                let r = Row::leaf(&line.label).detail(line.detail.clone());
                match &line.badge {
                    Some((t, c)) => r.badged(t.clone(), *c),
                    None => r,
                }
            })
            .collect();
        if self.cut() {
            rows.push(cut_row(match self.kind {
                // Sorted by last push, so what is missing is the stalest.
                Kind::Repos => "the most recently pushed \u{b7} there may be more",
                _ => "there may be more",
            }));
        }
        rows
    }

    fn pane(&mut self, sel: usize, cols: u16, _rows: u16) -> Pane {
        let mut lines = self.body(sel, cols);
        if !self.status.is_empty() {
            lines.push(String::new());
            lines.push(format!("{ORG}{}{OFF}", self.status));
        }
        Pane { image: None, lines }
    }

    fn on_action(&mut self, action: Action, sel: usize) -> Flow {
        self.status.clear();
        match action {
            Action::Key('o') => {
                let Some(url) = self.url_at(sel) else {
                    return Flow::Continue;
                };
                // Kept and shown. Dropped, a machine with no opener made `o`
                // a key that silently did nothing.
                self.status = crate::browse(&url);
                Flow::Dirty
            }
            Action::Key('r') => {
                self.fetch.refresh(Req::Json {
                    path: self.path.clone(),
                });
                Flow::Dirty
            }
            _ => Flow::Continue,
        }
    }

    fn footer(&self) -> String {
        // Only a row that IS a repo or a member has anything for `o` to open.
        match (self.kind, self.at(self.sel)) {
            (Kind::Repos, Some(_)) => "o browser \u{b7} r refresh".into(),
            (Kind::Members, Some(_)) => "o profile \u{b7} r refresh".into(),
            _ => "r refresh".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repo_row_badges_the_unusual_case_only() {
        let v: serde_json::Value = serde_json::from_str(
            r#"[{"name":"platform","private":true,"archived":false,"language":"Rust"},
                {"name":"brand","private":false,"archived":false},
                {"name":"old","private":true,"archived":true}]"#,
        )
        .expect("parses");
        let rows = build_rows(Kind::Repos, &v).expect("a repo list");
        // Private is the norm in these orgs, so it carries no badge.
        assert!(rows[0].badge.is_none(), "{:?}", rows[0].label);
        assert_eq!(rows[1].badge.as_ref().map(|b| b.0.as_str()), Some("public"));
        // Archived outranks public: it means "do not touch this".
        assert_eq!(
            rows[2].badge.as_ref().map(|b| b.0.as_str()),
            Some("archived")
        );
    }

    /// A default branch that is not main is worth saying; main is not.
    #[test]
    fn a_repo_row_names_an_unusual_default_branch() {
        let v: serde_json::Value = serde_json::from_str(
            r#"[{"name":"a","private":true,"archived":false,"default_branch":"dev"},
                {"name":"b","private":true,"archived":false,"default_branch":"main"}]"#,
        )
        .expect("parses");
        let rows = build_rows(Kind::Repos, &v).expect("a repo list");
        assert!(rows[0].detail.contains("dev"), "{:?}", rows[0].detail);
        assert!(!rows[1].detail.contains("main"), "{:?}", rows[1].detail);
    }

    #[test]
    fn a_team_with_no_description_falls_back_to_its_slug() {
        let v: serde_json::Value =
            serde_json::from_str(r#"[{"name":"Platform","slug":"platform"}]"#).expect("parses");
        let rows = build_rows(Kind::Teams, &v).expect("a team list");
        assert_eq!(rows[0].detail, "platform");
    }

    /// A payload that is not the shape we expected is an ERROR -- not a panic,
    /// and not an empty list either, which the screen would draw as "nothing
    /// here" about an org it simply failed to read.
    #[test]
    fn a_wrong_shaped_payload_is_an_error_rather_than_an_empty_list() {
        let v: serde_json::Value = serde_json::from_str(r#"{"message":"Not Found"}"#).unwrap();
        assert!(build_rows(Kind::Repos, &v).is_err());
        assert!(build_rows(Kind::Members, &v).is_err());
        assert!(build_rows(Kind::Teams, &v).is_err());

        let (mut p, _) = pane(Kind::Members, json(r#"{"message":"Not Found"}"#));
        let rows = p.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "could not read");
    }

    fn json(text: &str) -> Got {
        Got::Json(Arc::new(serde_json::from_str(text).expect("parses")))
    }

    fn logins(n: usize) -> Got {
        let all: Vec<String> = (0..n)
            .map(|i| format!(r#"{{"login":"o{i:03}"}}"#))
            .collect();
        json(&format!("[{}]", all.join(",")))
    }

    /// The org list over an answer already in hand.
    ///
    /// Everything the screen could go on to ask about those orgs is seeded as
    /// well -- each one's detail, and the three lists `enter` would open --
    /// so no path through a test reaches the network, including the path a
    /// regression would take.
    fn org_list(n: usize) -> OrgList {
        let fetch = Arc::new(Fetcher::new(""));
        fetch.seed(
            Req::Json {
                path: ORGS_PATH.into(),
            },
            logins(n),
        );
        for i in 0..n {
            let login = format!("o{i:03}");
            fetch.seed(OrgList::detail_req(&login), Got::Pending);
            for kind in [Kind::Repos, Kind::Members, Kind::Teams] {
                fetch.seed(
                    Req::Json {
                        path: list_path(&login, kind),
                    },
                    Got::Pending,
                );
            }
        }
        let mut l = OrgList::new(fetch);
        l.sync();
        l
    }

    /// Every row resolves to the org drawn on it, and a row that is not an
    /// org -- an error, a placeholder, the cut marker -- to nothing.
    fn list_agrees(l: &mut OrgList) {
        let rows = l.rows();
        for (i, row) in rows.iter().enumerate() {
            if let Some(o) = l.at(i) {
                assert_eq!(o.login, row.label, "row {i} read as another org");
            } else {
                assert!(
                    !row.label.starts_with("o0"),
                    "org row {i} resolves to nothing"
                );
            }
        }
        assert!(l.at(rows.len()).is_none());
    }

    #[test]
    fn the_org_list_reads_each_row_as_what_is_drawn_there() {
        let mut l = org_list(3);
        assert_eq!(l.rows().len(), 3);
        list_agrees(&mut l);

        // Empty: one placeholder row, which is not an org.
        let mut l = org_list(0);
        assert_eq!(l.rows().len(), 1);
        list_agrees(&mut l);
        assert!(matches!(l.on_action(Action::Activate, 0), Flow::Continue));
    }

    /// A refresh that fails keeps the last good list and draws one row saying
    /// so. `enter` on that row must not open the first org of a list that is
    /// not on screen.
    #[test]
    fn enter_on_the_error_row_does_not_open_an_org_nobody_can_see() {
        let mut l = org_list(3);
        l.fetch.seed(
            Req::Json {
                path: ORGS_PATH.into(),
            },
            Got::Failed("gh: Bad credentials (HTTP 401)".into()),
        );
        l.sync();
        assert_eq!(l.orgs.len(), 3, "the last good list is kept");
        let rows = l.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "could not list orgs");
        list_agrees(&mut l);
        assert!(matches!(l.on_action(Action::Activate, 0), Flow::Continue));
        assert_eq!(l.footer(), "r refresh");
    }

    /// A full page may not be every org, and the list ends in a row that says
    /// so -- a row that is not itself an org.
    #[test]
    fn a_full_page_of_orgs_says_it_may_have_been_cut() {
        assert!(ORGS_PATH.ends_with(&format!("per_page={PAGE}")));
        let mut l = org_list(PAGE);
        let rows = l.rows();
        assert_eq!(rows.len(), PAGE + 1);
        assert_eq!(rows[PAGE].label, "first 100 shown");
        list_agrees(&mut l);
        assert!(matches!(
            l.on_action(Action::Activate, PAGE),
            Flow::Continue
        ));
        assert!(!l.pane(PAGE, 80, 24).lines.is_empty());

        let mut l = org_list(PAGE - 1);
        assert_eq!(l.rows().len(), PAGE - 1, "a short page is the whole list");
    }

    /// An org whose detail request failed is not one still being fetched.
    #[test]
    fn a_failed_org_detail_does_not_read_as_fetching() {
        let mut l = org_list(2);
        l.fetch.seed(
            OrgList::detail_req("o000"),
            Got::Failed("gh: Server Error (HTTP 500)".into()),
        );
        let pane = l.pane(0, 80, 24).lines.join("\n");
        assert!(pane.contains("Server Error"), "{pane}");
        assert!(!pane.contains("fetching"), "{pane}");
        // The neighbour really is still on its way.
        assert!(l.pane(1, 80, 24).lines.join("\n").contains("fetching"));
    }

    /// One of an org's lists over an answer already in hand, and the fetcher
    /// it reads, for putting a different answer in afterwards.
    fn pane(kind: Kind, got: Got) -> (OrgPane, Arc<Fetcher>) {
        let fetch = Arc::new(Fetcher::new(""));
        fetch.seed(
            Req::Json {
                path: list_path("acme", kind),
            },
            got,
        );
        let mut p = OrgPane::make(Arc::clone(&fetch), "acme", kind);
        p.sync();
        (p, fetch)
    }

    fn pane_agrees(p: &mut OrgPane) {
        let rows = p.rows();
        for (i, row) in rows.iter().enumerate() {
            match p.at(i) {
                Some(line) => assert_eq!(line.label, row.label, "row {i} read as another"),
                None => assert!(p.url_at(i).is_none(), "row {i} is nothing but opens"),
            }
        }
        assert!(p.at(rows.len()).is_none());
    }

    #[test]
    fn an_org_list_reads_each_row_as_what_is_drawn_there() {
        let (mut p, _) = pane(Kind::Members, logins(3));
        assert_eq!(p.rows().len(), 3);
        pane_agrees(&mut p);
        assert_eq!(p.url_at(1).as_deref(), Some("https://github.com/o001"));

        // Empty: one placeholder, and nothing for `o` to open.
        let (mut p, _) = pane(Kind::Members, json("[]"));
        assert_eq!(p.rows()[0].label, "nothing here");
        pane_agrees(&mut p);
        assert!(p.url_at(0).is_none());
        assert_eq!(p.footer(), "r refresh");
    }

    /// After a failed refresh the one row drawn is the error, and `o` on it
    /// must not open the first member of a list that is no longer on screen.
    #[test]
    fn o_on_the_error_row_opens_nothing() {
        let (mut p, fetch) = pane(Kind::Members, logins(3));
        fetch.seed(
            Req::Json {
                path: list_path("acme", Kind::Members),
            },
            Got::Failed("gh: Server Error (HTTP 500)".into()),
        );
        p.sync();
        assert_eq!(p.rows.len(), 3, "the last good list is kept");
        let rows = p.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "could not read");
        pane_agrees(&mut p);
        assert!(p.url_at(0).is_none());
        assert!(matches!(p.on_action(Action::Key('o'), 0), Flow::Continue));
        assert!(p.status.is_empty());
    }

    /// A hundred back is a full page, so the list says it may be short; the
    /// row that says so is not a member and `o` does nothing on it.
    #[test]
    fn a_full_page_says_it_may_have_been_cut() {
        let (mut p, _) = pane(Kind::Members, logins(PAGE));
        let rows = p.rows();
        assert_eq!(rows.len(), PAGE + 1);
        assert_eq!(rows[PAGE].label, "first 100 shown");
        pane_agrees(&mut p);
        assert!(p.url_at(PAGE).is_none());
        assert!(p.pane(PAGE, 80, 24).lines.join("\n").contains("full page"));

        let (mut p, _) = pane(Kind::Members, logins(PAGE - 1));
        assert_eq!(p.rows().len(), PAGE - 1, "a short page is the whole list");
    }

    /// What `o` reported is shown. A machine with no opener otherwise has a
    /// key that does nothing and says nothing.
    #[test]
    fn what_the_browser_key_reported_is_on_screen() {
        let (mut p, _) = pane(Kind::Members, logins(2));
        p.status = "could not run xdg-open: No such file or directory".into();
        let text = p.pane(0, 80, 24).lines.join("\n");
        assert!(text.contains("could not run xdg-open"), "{text}");
        // And it goes when the cursor moves on, so it is never about a row
        // other than the one it happened on.
        p.focus(1);
        assert!(!p.pane(1, 80, 24).lines.join("\n").contains("xdg-open"));
    }
}
