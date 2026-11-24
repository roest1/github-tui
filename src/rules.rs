//! The effective branch rules: what actually applies to one branch, and who
//! can ignore it.
//!
//! The last of the bash screens to arrive, and the smallest, because it is
//! read-only. Two endpoints: `rules/branches/{branch}` says which rules reach
//! this branch and which ruleset each came from, and `rulesets/{id}` says who
//! may bypass that ruleset.
//!
//! ## Why the bypass is the point
//!
//! `protected: true` on a branch, which the table already shows, answers a
//! weaker question than it looks like it does. A ruleset with a bypass actor
//! turns every rule under it into guidance that somebody -- possibly you --
//! can push straight past. So the bypass goes in the group's DETAIL, where it
//! is readable without selecting anything, and the actors go in the pane.
//! That difference is the whole reason this screen is worth a round trip.
//!
//! ## "No rules" is not "unprotected by mistake"
//!
//! An empty answer means no ruleset TARGETS this branch, which for a feature
//! branch is the normal and correct state. The bash version said so in as many
//! words and that sentence is ported, because an empty screen here otherwise
//! reads as a finding.
//!
//! ## The parameters render generically
//!
//! Two rule types get a hand-written summary, the two worth reading at a
//! glance -- `pull_request` and `required_status_checks`. Everything else is
//! rendered from whatever JSON came back, so a rule type GitHub adds later
//! shows up with its settings rather than as a bare name. `copilot_code_review`
//! already arrived that way and needed no code.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;
use tui_core::{nav::Action, Flow, Pane, Row, Screen, Tick};

use crate::gh::{self, Fetcher, Got, Req};
use crate::paint::{idx, BOLD, DIM, GRY, OFF, ORG, RED, YEL};

/// One rule, as `rules/branches/{branch}` returns it.
#[derive(Debug, Clone, Deserialize)]
struct Rule {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    ruleset_id: u64,
    #[serde(default)]
    ruleset_source: Option<String>,
    #[serde(default)]
    ruleset_source_type: Option<String>,
    #[serde(default)]
    parameters: Option<Value>,
}

/// Who may push past a ruleset.
#[derive(Debug, Clone, Deserialize)]
struct Bypass {
    #[serde(default)]
    actor_type: Option<String>,
    #[serde(default)]
    actor_id: Option<u64>,
    #[serde(default)]
    bypass_mode: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RulesetInfo {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    enforcement: Option<String>,
    /// `None` when the answer did not say. GitHub leaves the field out
    /// unless the caller may edit the ruleset, so on someone else's repo
    /// that is the usual answer -- and read as an empty list it drew "no
    /// bypass -- absolute" over a ruleset whose bypass list was merely not
    /// shown to us.
    #[serde(default)]
    bypass_actors: Option<Vec<Bypass>>,
}

/// One ruleset's contribution to this branch.
struct Set {
    id: u64,
    source: Option<String>,
    source_type: Option<String>,
    rules: Vec<Rule>,
    /// `None` until `rulesets/{id}` lands, which is a second round trip -- so
    /// the group draws with its rules first and gains the bypass after.
    info: Option<RulesetInfo>,
    /// Why there is no `info`, when there is a reason rather than a wait.
    info_error: Option<String>,
}

/// A branch name inside a URL PATH, not a query string.
///
/// `/` is left alone, which is measured rather than assumed: asking for
/// `rules/branches/main/x` on a repo whose `main` has four rules answers `[]`
/// rather than 404, so the route takes the branch as the rest of the path and
/// a slashed name needs no encoding. Everything else outside the unreserved
/// set is escaped, because `#` is legal in a branch name and would otherwise
/// start a fragment.
fn path_branch(name: &str) -> String {
    gh::percent(name, b"/")
}

pub(crate) fn rules_req(repo: &str, branch: &str) -> Req {
    Req::Json {
        path: format!("repos/{repo}/rules/branches/{}", path_branch(branch)),
    }
}

fn set_req(repo: &str, id: u64) -> Req {
    Req::Json {
        path: format!("repos/{repo}/rulesets/{id}"),
    }
}

/// The one-line summary beside a rule's name.
///
/// Hand-written for the two that are worth reading without selecting, and
/// derived from the JSON for everything else.
fn summary(kind: &str, p: Option<&Value>) -> String {
    let Some(p) = p else { return String::new() };
    match kind {
        "pull_request" => {
            let mut bits = Vec::new();
            let n = p
                .get("required_approving_review_count")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            bits.push(match n {
                0 => "no approvals".to_string(),
                1 => "1 approval".to_string(),
                n => format!("{n} approvals"),
            });
            if p.get("require_code_owner_review") == Some(&Value::Bool(true)) {
                bits.push("code owners".into());
            }
            if p.get("dismiss_stale_reviews_on_push") == Some(&Value::Bool(true)) {
                bits.push("dismiss stale".into());
            }
            if p.get("require_last_push_approval") == Some(&Value::Bool(true)) {
                bits.push("last push too".into());
            }
            bits.join(" \u{b7} ")
        }
        "required_status_checks" => {
            let n = p
                .get("required_status_checks")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0);
            let mut s = match n {
                1 => "1 check".to_string(),
                n => format!("{n} checks"),
            };
            if p.get("strict_required_status_checks_policy") == Some(&Value::Bool(true)) {
                s.push_str(" \u{b7} strict");
            }
            s
        }
        // Generic: the settings that are ON, named. A rule type added later
        // says something rather than nothing, which is how
        // `copilot_code_review` arrived needing no code of its own.
        _ => {
            let Some(obj) = p.as_object() else {
                return String::new();
            };
            let mut bits = Vec::new();
            for (k, v) in obj {
                match v {
                    Value::Bool(true) => bits.push(human(k)),
                    Value::Number(n) => bits.push(format!("{} {n}", human(k))),
                    Value::String(s) if !s.is_empty() => bits.push(format!("{}: {s}", human(k))),
                    Value::Array(a) if !a.is_empty() => {
                        bits.push(format!("{} ({})", human(k), a.len()))
                    }
                    _ => {}
                }
            }
            bits.join(" \u{b7} ")
        }
    }
}

/// `required_approving_review_count` -> `required approving review count`.
fn human(key: &str) -> String {
    key.replace('_', " ")
}

/// Every parameter, one per line, for the pane.
///
/// Unlike `summary` this shows the settings that are OFF too: in the pane
/// there is room, and "not strict" is a fact you came here to learn.
fn lines_for(p: Option<&Value>) -> Vec<String> {
    let mut out = Vec::new();
    let Some(obj) = p.and_then(Value::as_object) else {
        out.push(format!(
            "{DIM}no settings \u{2014} the rule is the whole of it{OFF}"
        ));
        return out;
    };
    for (k, v) in obj {
        match v {
            Value::Array(a) => {
                out.push(format!("{GRY}{}{OFF}  {}", human(k), a.len()));
                // The required checks are the useful case and the reason this
                // expands an array at all: "which checks" is most of why
                // anyone opens a `required_status_checks` rule.
                for item in a {
                    let label = item
                        .get("context")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| item.as_str().map(str::to_string))
                        .unwrap_or_else(|| item.to_string());
                    out.push(format!("    {ORG}{label}{OFF}"));
                }
            }
            Value::Bool(b) => out.push(format!(
                "{GRY}{}{OFF}  {}",
                human(k),
                if *b {
                    format!("{YEL}yes{OFF}")
                } else {
                    format!("{DIM}no{OFF}")
                }
            )),
            Value::Null => out.push(format!("{GRY}{}{OFF}  {DIM}none{OFF}", human(k))),
            other => out.push(format!("{GRY}{}{OFF}  {other}", human(k))),
        }
    }
    if out.is_empty() {
        out.push(format!("{DIM}no settings on this rule{OFF}"));
    }
    out
}

#[derive(Debug, PartialEq, Eq)]
enum At {
    Nothing,
    Head(usize),
    Rule(usize, usize),
}

pub struct Rules {
    repo: String,
    branch: String,
    fetch: Arc<Fetcher>,
    sets: Vec<Set>,
    open: Vec<bool>,
    /// True once the rules call has answered, so an empty list can tell "no
    /// rules apply" apart from "not back yet".
    answered: bool,
    error: Option<String>,
    epoch_seen: u64,
    sel: usize,
}

impl Rules {
    pub fn open(repo: &str, branch: &str, fetch: Arc<Fetcher>) -> Self {
        let me = Self {
            repo: repo.to_string(),
            branch: branch.to_string(),
            fetch,
            sets: Vec::new(),
            open: Vec::new(),
            answered: false,
            error: None,
            epoch_seen: 0,
            sel: 0,
        };
        me.fetch.request(rules_req(&me.repo, &me.branch));
        me
    }

    fn sync(&mut self) {
        match self.fetch.get(&rules_req(&self.repo, &self.branch)) {
            Some(Got::Json(v)) => {
                self.answered = true;
                match Vec::<Rule>::deserialize(v.as_ref()) {
                    Ok(rules) => {
                        self.error = None;
                        self.group(rules);
                    }
                    Err(e) => self.error = Some(format!("unreadable rules: {e}")),
                }
            }
            Some(Got::Denied(d)) => {
                self.answered = true;
                self.error = Some(d.fix());
            }
            Some(Got::Failed(e)) => {
                self.answered = true;
                self.error = Some(e);
            }
            _ => {}
        }
        // The second round trip, one per ruleset, fired as the first answer
        // names them -- the same shape the Secrets screen uses to grow a lane
        // per environment.
        //
        // Read again on EVERY pass, including for a set that already has an
        // answer. Skipping those meant `r` re-asked each ruleset and then
        // never looked at what came back, so a bypass added since stayed
        // "no bypass" until the screen was closed and opened again.
        for i in 0..self.sets.len() {
            let req = set_req(&self.repo, self.sets[i].id);
            let (info, error) = match self.fetch.get(&req) {
                Some(Got::Json(v)) => match RulesetInfo::deserialize(v.as_ref()) {
                    Ok(info) => (Some(info), None),
                    Err(e) => (None, Some(format!("unreadable ruleset: {e}"))),
                },
                // A ruleset you cannot read is not a failure of this screen:
                // the rules above it still apply and are still listed. Only
                // the bypass line is unknown, and it says so.
                Some(Got::Denied(d)) => (None, Some(d.fix())),
                Some(Got::Failed(e)) => (None, Some(e)),
                // Not asked yet, or not back yet.
                _ => {
                    self.fetch.request(req);
                    continue;
                }
            };
            self.sets[i].info = info;
            self.sets[i].info_error = error;
        }
    }

    /// Rules into rulesets, in the order GitHub listed them.
    ///
    /// Order rather than id, because the API returns the rules grouped the way
    /// it evaluates them and reordering by number would invent a precedence.
    fn group(&mut self, rules: Vec<Rule>) {
        let mut sets: Vec<Set> = Vec::new();
        for r in rules {
            match sets.iter_mut().find(|s| s.id == r.ruleset_id) {
                Some(s) => s.rules.push(r),
                None => sets.push(Set {
                    id: r.ruleset_id,
                    source: r.ruleset_source.clone(),
                    source_type: r.ruleset_source_type.clone(),
                    rules: vec![r],
                    info: None,
                    info_error: None,
                }),
            }
        }
        // Carry over whatever the previous pass had already fetched, so a
        // refresh of the rules does not throw away the bypass answers.
        for s in sets.iter_mut() {
            if let Some(old) = self.sets.iter().find(|o| o.id == s.id) {
                s.info = old.info.clone();
                s.info_error = old.info_error.clone();
            }
        }
        if self.open.len() != sets.len() {
            self.open = vec![true; sets.len()];
        }
        self.sets = sets;
    }

    /// What the group head says about the bypass without being selected.
    ///
    /// The strong claim is the one worth making loudly: no bypass means the
    /// rules below are absolute. Anything else is a number, because naming
    /// the actors needs the pane.
    fn bypass_detail(s: &Set) -> (String, u8) {
        match (&s.info, &s.info_error) {
            (Some(i), _) => match i.bypass_actors.as_ref().map(Vec::len) {
                // The ruleset answered and kept its bypass list to itself.
                // That is not an empty list.
                None => ("bypass unknown".into(), idx::DIM),
                Some(0) => ("no bypass \u{2014} absolute".into(), idx::GRN),
                Some(1) => ("1 can bypass".into(), idx::YEL),
                Some(n) => (format!("{n} can bypass"), idx::YEL),
            },
            (None, Some(_)) => ("bypass unknown".into(), idx::DIM),
            (None, None) => ("asking\u{2026}".into(), idx::DIM),
        }
    }

    /// The row a ruleset's heading is drawn on.
    fn head_row(&self, g: usize) -> usize {
        self.sets
            .iter()
            .zip(&self.open)
            .take(g)
            // The heading, its rules if it is open, and the spacer.
            .map(|(s, open)| 2 + if *open { s.rules.len() } else { 0 })
            .sum()
    }

    fn at(&self, sel: usize) -> At {
        // The one row drawn for a failure is not the first ruleset. After a
        // refresh failed the rulesets are still held here, and `enter` on
        // "could not read the rules" opened and closed one out of sight.
        if self.error.is_some() {
            return At::Nothing;
        }
        let mut row = 0;
        for (g, s) in self.sets.iter().enumerate() {
            if sel == row {
                return At::Head(g);
            }
            row += 1;
            if self.open[g] {
                for k in 0..s.rules.len() {
                    if sel == row {
                        return At::Rule(g, k);
                    }
                    row += 1;
                }
            }
            row += 1; // spacer
        }
        At::Nothing
    }
}

impl Screen for Rules {
    fn title(&self) -> String {
        format!("{} \u{b7} rules on {}", self.repo, self.branch)
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
            return vec![Row::leaf("could not read the rules").detail(e.clone())];
        }
        if self.sets.is_empty() {
            // The sentence that matters, ported from bash: an empty answer is
            // the normal state for a feature branch, and a blank screen here
            // would read as a finding.
            return vec![Row::leaf(if self.answered {
                format!("no rules apply to {}", self.branch)
            } else {
                "asking github\u{2026}".to_string()
            })
            .detail(if self.answered {
                "a ruleset may simply not target it"
            } else {
                ""
            })];
        }
        let mut rows = Vec::new();
        for (g, s) in self.sets.iter().enumerate() {
            let name = s
                .info
                .as_ref()
                .and_then(|i| i.name.clone())
                .unwrap_or_else(|| format!("ruleset {}", s.id));
            let mut head = Row::leaf(format!("\u{1f6e1} {name}"));
            head.group = true;
            head.expanded = self.open[g];
            let (bypass, colour) = Self::bypass_detail(s);
            // The bypass alone, because the 52-cell column truncates and this
            // is the half you came for -- `Repository · no bypass — absolute`
            // came out as `Repository · …`, which is the wrong word surviving.
            //
            // The source is named only when it is NOT this repository, which
            // is the rule the root screen already follows: a row says the
            // unusual thing and stays quiet otherwise. An org ruleset is worth
            // flagging because this repo's settings page cannot even show it.
            let detail = match s.source_type.as_deref() {
                Some("Repository") | None => bypass,
                Some(other) => format!("{} \u{b7} {bypass}", other.to_lowercase()),
            };
            rows.push(
                head.detail(detail)
                    .badged(format!("{:>2}", s.rules.len()), colour),
            );
            if self.open[g] {
                for r in &s.rules {
                    let mut row = Row::leaf(r.kind.replace('_', " "));
                    row.depth = 1;
                    rows.push(row.detail(summary(&r.kind, r.parameters.as_ref())));
                }
            }
            rows.push(Row::spacer());
        }
        rows
    }

    fn pane(&mut self, sel: usize, _cols: u16, _rows: u16) -> Pane {
        let mut lines: Vec<String> = Vec::new();
        if let Some(e) = &self.error {
            lines.push(format!("{RED}{e}{OFF}"));
            return Pane { image: None, lines };
        }
        match self.at(sel) {
            At::Nothing => {
                if self.sets.is_empty() && self.answered {
                    lines.push(format!("{BOLD}Nothing rules {}{OFF}", self.branch));
                    lines.push(String::new());
                    lines.push("No ruleset targets this branch.".into());
                    lines.push(String::new());
                    lines.push(format!("{DIM}Which is not the same as unprotected by{OFF}"));
                    lines.push(format!(
                        "{DIM}mistake. A ruleset is written against a ref{OFF}"
                    ));
                    lines.push(format!(
                        "{DIM}pattern, and a feature branch is normally{OFF}"
                    ));
                    lines.push(format!("{DIM}outside every one of them.{OFF}"));
                }
            }
            At::Head(g) => {
                let s = &self.sets[g];
                if let Some(i) = &s.info {
                    lines.push(format!(
                        "{BOLD}{}{OFF}",
                        i.name
                            .clone()
                            .unwrap_or_else(|| format!("ruleset {}", s.id))
                    ));
                } else {
                    lines.push(format!("{BOLD}ruleset {}{OFF}", s.id));
                }
                lines.push(format!(
                    "{DIM}{} \u{b7} id {}{OFF}",
                    s.source.as_deref().unwrap_or("unknown source"),
                    s.id
                ));
                if let Some(e) = s.info.as_ref().and_then(|i| i.enforcement.clone()) {
                    lines.push(format!("{GRY}enforcement{OFF}  {e}"));
                }
                lines.push(String::new());
                let actors = s.info.as_ref().map(|i| i.bypass_actors.as_deref());
                match (actors, &s.info_error) {
                    // Answered, without the list. GitHub shows it only to
                    // someone who may edit the ruleset.
                    (Some(None), _) => {
                        lines.push(format!("{RED}Who can bypass it is unknown.{OFF}"));
                        lines.push(String::new());
                        lines.push(format!(
                            "{DIM}GitHub lists the bypass actors only for someone{OFF}"
                        ));
                        lines.push(format!(
                            "{DIM}who can edit the ruleset, and left them out.{OFF}"
                        ));
                    }
                    (Some(Some([])), _) => {
                        lines.push(format!("{ORG}Nobody can bypass it.{OFF}"));
                        lines.push(String::new());
                        lines.push(format!(
                            "{DIM}So the rules below are absolute, not advice.{OFF}"
                        ));
                    }
                    (Some(Some(actors)), _) => {
                        lines.push(format!("{YEL}Can bypass:{OFF}"));
                        for a in actors {
                            lines.push(format!(
                                "  {} {}  {DIM}{}{OFF}",
                                a.actor_type.as_deref().unwrap_or("?"),
                                a.actor_id.map(|n| n.to_string()).unwrap_or_default(),
                                mode(a.bypass_mode.as_deref()),
                            ));
                        }
                        lines.push(String::new());
                        lines.push(format!(
                            "{DIM}A bypass turns every rule below into guidance{OFF}"
                        ));
                        lines.push(format!(
                            "{DIM}somebody can push straight past. That is the{OFF}"
                        ));
                        lines.push(format!(
                            "{DIM}difference between \"protected\" and knowing{OFF}"
                        ));
                        lines.push(format!("{DIM}what it buys you.{OFF}"));
                    }
                    // The rules still apply and are still listed; it is only
                    // the bypass that is unknown, and an unknown bypass must
                    // not read as none.
                    (None, Some(e)) => {
                        lines.push(format!("{RED}Who can bypass it is unknown.{OFF}"));
                        lines.push(String::new());
                        lines.push(format!("{DIM}{e}{OFF}"));
                    }
                    (None, None) => lines.push(format!("{DIM}asking github\u{2026}{OFF}")),
                }
            }
            At::Rule(g, k) => {
                let r = &self.sets[g].rules[k];
                lines.push(format!("{BOLD}{}{OFF}", r.kind.replace('_', " ")));
                lines.push(format!(
                    "{DIM}from {}{OFF}",
                    r.ruleset_source.as_deref().unwrap_or("unknown source")
                ));
                lines.push(String::new());
                lines.extend(lines_for(r.parameters.as_ref()));
            }
        }
        Pane { image: None, lines }
    }

    fn focus(&mut self, sel: usize) {
        self.sel = sel;
    }

    /// Where the cursor belongs after `Flow::Retarget`: where it was last
    /// put, by the app or by a group closing under it.
    fn initial_sel(&self) -> usize {
        self.sel
    }

    fn on_action(&mut self, action: Action, sel: usize) -> Flow {
        match (action, self.at(sel)) {
            // `l` opens a closed ruleset and does nothing else, which is what
            // sends the app on into the pane. Sharing `enter`'s toggle, it
            // closed an open one instead and the pane -- where the bypass
            // actors are -- could not be reached from a heading at all.
            (Action::Open, At::Head(g)) if !self.open[g] => {
                self.open[g] = true;
                Flow::Dirty
            }
            (Action::Activate, At::Head(g)) => {
                self.open[g] = !self.open[g];
                Flow::Dirty
            }
            (Action::Close, At::Head(g)) => {
                self.open[g] = false;
                Flow::Dirty
            }
            // Closing from a rule takes the row the cursor is on away, so the
            // cursor goes to the heading. Left where it was it sat on
            // whatever moved up into that row: from the second rule down,
            // the NEXT ruleset's heading.
            (Action::Close, At::Rule(g, _)) => {
                self.open[g] = false;
                self.sel = self.head_row(g);
                Flow::Retarget
            }
            (Action::Key('o'), _) => {
                // The repo's rules page, not a ruleset's: an org-sourced
                // ruleset has no page under this repo's settings at all, so
                // the list is the one URL that is always right.
                let url = format!("https://github.com/{}/settings/rules", self.repo);
                crate::browse(&url);
                Flow::Dirty
            }
            (Action::Key('r'), _) => {
                self.fetch.refresh(rules_req(&self.repo, &self.branch));
                for s in &self.sets {
                    self.fetch.refresh(set_req(&self.repo, s.id));
                }
                Flow::Dirty
            }
            _ => Flow::Continue,
        }
    }

    fn footer(&self) -> String {
        // Nothing to open when nothing applies or nothing could be read, and a
        // footer offering a key that does nothing is the same small lie as a
        // filter hiding rows.
        if self.sets.is_empty() || self.error.is_some() {
            return "o browser \u{b7} r refresh".into();
        }
        "enter open/close \u{b7} o browser \u{b7} r refresh".into()
    }
}

/// What a `bypass_mode` means, in words.
///
/// The two documented values. An unknown one is printed rather than
/// translated, which is the same direction `Verdict` fails in: a value GitHub
/// adds later should read as itself, not as a guess.
fn mode(m: Option<&str>) -> String {
    match m {
        Some("always") => "always".into(),
        Some("pull_request") => "through a pull request".into(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real answer for `roest1/dotfiles` main, trimmed to the shape rather
    /// than the whole of its eleven checks.
    const RAW: &str = r#"[
      {"type":"deletion","ruleset_source_type":"Repository",
       "ruleset_source":"roest1/dotfiles","ruleset_id":20169814},
      {"type":"non_fast_forward","ruleset_source_type":"Repository",
       "ruleset_source":"roest1/dotfiles","ruleset_id":20169814},
      {"type":"pull_request","parameters":{"required_approving_review_count":1,
       "dismiss_stale_reviews_on_push":true,"required_reviewers":[],
       "require_code_owner_review":true,"require_last_push_approval":false},
       "ruleset_source_type":"Repository","ruleset_source":"roest1/dotfiles",
       "ruleset_id":20169814},
      {"type":"required_status_checks","parameters":{
       "strict_required_status_checks_policy":true,
       "required_status_checks":[{"context":"shellcheck","integration_id":15368},
                                 {"context":"manifest parses","integration_id":15368}]},
       "ruleset_source_type":"Repository","ruleset_source":"roest1/dotfiles",
       "ruleset_id":20169814},
      {"type":"copilot_code_review","parameters":{"review_on_push":false,
       "review_draft_pull_requests":true},"ruleset_source_type":"Organization",
       "ruleset_source":"some-org","ruleset_id":99}
    ]"#;

    fn screen() -> Rules {
        let mut r = Rules {
            repo: "roest1/dotfiles".into(),
            branch: "main".into(),
            fetch: Arc::new(Fetcher::new("roest1/dotfiles")),
            sets: Vec::new(),
            open: Vec::new(),
            answered: true,
            error: None,
            epoch_seen: 0,
            sel: 0,
        };
        let rules: Vec<Rule> = serde_json::from_str(RAW).expect("parses");
        r.group(rules);
        r
    }

    #[test]
    fn the_real_payload_parses_and_groups_by_ruleset() {
        let r = screen();
        assert_eq!(r.sets.len(), 2, "two rulesets reach this branch");
        assert_eq!(r.sets[0].id, 20169814);
        assert_eq!(r.sets[0].rules.len(), 4);
        assert_eq!(r.sets[1].id, 99);
        assert_eq!(r.sets[1].source_type.as_deref(), Some("Organization"));
    }

    /// The two summaries worth reading without selecting anything.
    #[test]
    fn the_summaries_say_the_number_that_matters() {
        let r = screen();
        let pr = r.sets[0]
            .rules
            .iter()
            .find(|x| x.kind == "pull_request")
            .unwrap();
        let s = summary(&pr.kind, pr.parameters.as_ref());
        assert!(s.contains("1 approval"), "{s}");
        assert!(s.contains("code owners"), "{s}");
        // A false setting is not listed in the SUMMARY -- it would be noise in
        // a line whose job is the headline.
        assert!(!s.contains("last push"), "{s}");

        let cs = r.sets[0]
            .rules
            .iter()
            .find(|x| x.kind == "required_status_checks")
            .unwrap();
        let s = summary(&cs.kind, cs.parameters.as_ref());
        assert!(s.contains("2 checks") && s.contains("strict"), "{s}");
    }

    /// A rule type nobody wrote code for still says what it does. This is the
    /// case that actually turned up: `copilot_code_review` did not exist when
    /// the bash version was written.
    #[test]
    fn an_unknown_rule_type_still_renders_its_settings() {
        let r = screen();
        let c = &r.sets[1].rules[0];
        let s = summary(&c.kind, c.parameters.as_ref());
        assert!(s.contains("review draft pull requests"), "{s}");
        assert!(!s.contains("review on push"), "a false setting leaked: {s}");
    }

    /// The pane shows the settings that are OFF too, which the summary hides.
    #[test]
    fn the_pane_shows_what_is_switched_off_as_well() {
        let r = screen();
        let pr = r.sets[0]
            .rules
            .iter()
            .find(|x| x.kind == "pull_request")
            .unwrap();
        let body = lines_for(pr.parameters.as_ref()).join("\n");
        assert!(body.contains("require last push approval"), "{body}");
        assert!(body.contains("no"), "{body}");
        // And an array is expanded, because "which checks" is most of why
        // anyone opens that rule.
        let cs = r.sets[0]
            .rules
            .iter()
            .find(|x| x.kind == "required_status_checks")
            .unwrap();
        let body = lines_for(cs.parameters.as_ref()).join("\n");
        assert!(
            body.contains("shellcheck") && body.contains("manifest parses"),
            "{body}"
        );
    }

    /// An unknown bypass must never render as "no bypass". One says the rules
    /// are absolute; the other says we could not find out.
    #[test]
    fn an_unreadable_ruleset_does_not_read_as_nobody_can_bypass() {
        let mut r = screen();
        r.sets[0].info = Some(RulesetInfo {
            name: Some("main-branch-rules".into()),
            enforcement: Some("active".into()),
            bypass_actors: Some(Vec::new()),
        });
        let (d, _) = Rules::bypass_detail(&r.sets[0]);
        assert!(d.contains("no bypass"), "{d}");

        r.sets[1].info_error = Some("needs admin".into());
        let (d, _) = Rules::bypass_detail(&r.sets[1]);
        assert!(d.contains("unknown"), "{d}");
        assert!(!d.contains("no bypass"), "{d}");

        // And before either answer lands it is neither.
        let waiting = Set {
            id: 1,
            source: None,
            source_type: None,
            rules: Vec::new(),
            info: None,
            info_error: None,
        };
        let (d, _) = Rules::bypass_detail(&waiting);
        assert!(d.contains("asking"), "{d}");
    }

    /// GitHub leaves `bypass_actors` out for anyone who cannot edit the
    /// ruleset. A list that was not sent is not an empty list: only an
    /// answer that says `[]` may claim nobody can bypass.
    #[test]
    fn a_ruleset_that_keeps_its_bypass_list_to_itself_is_unknown() {
        let mut r = screen();
        let told = |json: &str| serde_json::from_str::<RulesetInfo>(json).expect("parses");

        for kept in [
            r#"{"name":"main-branch-rules","enforcement":"active"}"#,
            r#"{"name":"main-branch-rules","enforcement":"active","bypass_actors":null}"#,
        ] {
            r.sets[0].info = Some(told(kept));
            let (d, _) = Rules::bypass_detail(&r.sets[0]);
            assert_eq!(d, "bypass unknown", "{kept}");
            assert!(r.rows()[0]
                .detail
                .as_deref()
                .unwrap_or("")
                .contains("unknown"));
            let pane = r.pane(0, 60, 40).lines.join("\n");
            assert!(pane.contains("Who can bypass it is unknown"), "{pane}");
            assert!(!pane.contains("Nobody can bypass"), "{pane}");
            // The rest of the answer is still used.
            assert!(pane.contains("main-branch-rules"), "{pane}");
        }

        r.sets[0].info = Some(told(r#"{"name":"x","bypass_actors":[]}"#));
        let (d, _) = Rules::bypass_detail(&r.sets[0]);
        assert!(d.contains("no bypass"), "{d}");
        assert!(r
            .pane(0, 60, 40)
            .lines
            .join("\n")
            .contains("Nobody can bypass"));

        r.sets[0].info = Some(told(
            r#"{"name":"x","bypass_actors":[{"actor_type":"Team","actor_id":7,"bypass_mode":"always"}]}"#,
        ));
        let (d, _) = Rules::bypass_detail(&r.sets[0]);
        assert_eq!(d, "1 can bypass");
        assert!(r.pane(0, 60, 40).lines.join("\n").contains("Team 7"));
    }

    /// `r` asks for each ruleset again, and what comes back is read -- in
    /// both directions. A bypass added since must not stay "no bypass", and
    /// a ruleset that could not be read and now can must not stay unknown.
    #[test]
    fn a_refreshed_ruleset_replaces_what_was_known_about_it() {
        let mut r = screen();
        let json = |raw: &str| {
            Got::Json(Arc::new(
                serde_json::from_str::<Value>(raw).expect("parses"),
            ))
        };
        r.fetch.seed(rules_req(&r.repo, &r.branch), json(RAW));
        r.fetch.seed(
            set_req(&r.repo, 20169814),
            json(r#"{"name":"main","bypass_actors":[]}"#),
        );
        r.fetch
            .seed(set_req(&r.repo, 99), Got::Failed("HTTP 502".into()));
        r.sync();
        assert!(Rules::bypass_detail(&r.sets[0]).0.contains("no bypass"));
        assert_eq!(Rules::bypass_detail(&r.sets[1]).0, "bypass unknown");

        // What a refresh leaves in the fetcher once it lands.
        r.fetch.seed(
            set_req(&r.repo, 20169814),
            json(r#"{"name":"main","bypass_actors":[{"actor_type":"Team","actor_id":7}]}"#),
        );
        r.fetch.seed(
            set_req(&r.repo, 99),
            json(r#"{"name":"org-wide","bypass_actors":[]}"#),
        );
        r.sync();
        assert_eq!(
            Rules::bypass_detail(&r.sets[0]).0,
            "1 can bypass",
            "the answer from before the refresh was kept"
        );
        assert!(
            Rules::bypass_detail(&r.sets[1]).0.contains("no bypass"),
            "a ruleset that failed once stayed unknown"
        );

        // And one that stops being readable says so rather than repeating
        // the last thing it was told.
        r.fetch
            .seed(set_req(&r.repo, 20169814), Got::Failed("HTTP 502".into()));
        r.sync();
        assert_eq!(Rules::bypass_detail(&r.sets[0]).0, "bypass unknown");
    }

    /// `l` opens a closed ruleset and otherwise does nothing, so the app
    /// moves on into the pane. Only `enter` closes one from its heading.
    #[test]
    fn l_opens_a_ruleset_and_never_closes_one() {
        let mut r = screen();
        assert!(r.open[0]);
        assert!(matches!(r.on_action(Action::Open, 0), Flow::Continue));
        assert!(r.open[0], "l closed an open ruleset");
        assert!(matches!(r.on_action(Action::Open, 1), Flow::Continue));

        assert!(matches!(r.on_action(Action::Activate, 0), Flow::Dirty));
        assert!(!r.open[0]);
        assert!(matches!(r.on_action(Action::Open, 0), Flow::Dirty));
        assert!(r.open[0]);
    }

    /// `h` on a rule closes its ruleset and takes the cursor to the heading.
    /// The row it was on is gone, and what moves up into it belongs to the
    /// next ruleset.
    #[test]
    fn closing_from_a_rule_puts_the_cursor_on_its_heading() {
        for (g, k) in [(0, 0), (0, 2), (0, 3), (1, 0)] {
            let mut r = screen();
            let from = (0..r.rows().len())
                .find(|&i| r.at(i) == At::Rule(g, k))
                .expect("the rule is drawn");
            r.focus(from);
            assert!(matches!(r.on_action(Action::Close, from), Flow::Retarget));
            assert!(!r.open[g]);
            let to = r.initial_sel();
            assert_eq!(r.at(to), At::Head(g), "from rule {k} of ruleset {g}");
            assert!(r.rows()[to].group);
        }
    }

    /// The `At` walk and `rows()` are two readings of one list, in every shape
    /// this screen has -- including the empty one, which draws a row.
    #[test]
    fn the_walk_matches_the_rows() {
        let mut r = screen();
        for shape in [vec![true, true], vec![false, true], vec![false, false]] {
            r.open = shape.clone();
            let rows = r.rows();
            for (i, row) in rows.iter().enumerate() {
                if !row.selectable {
                    continue;
                }
                match r.at(i) {
                    At::Head(g) => assert!(
                        row.group && row.label.contains(&group_name(&r, g)),
                        "row {i} is not head {g}: {:?}",
                        row.label
                    ),
                    At::Rule(g, k) => assert_eq!(
                        row.label,
                        r.sets[g].rules[k].kind.replace('_', " "),
                        "row {i} is not rule {g}/{k}"
                    ),
                    At::Nothing => panic!("row {i} ({:?}) walks to nothing", row.label),
                }
            }
        }
        // A refresh that failed draws one row over rulesets that are still
        // held, and nothing walks to it either -- so no key opens or closes
        // one of them out of sight.
        r.open = vec![true, true];
        r.error = Some("HTTP 502".into());
        assert_eq!(r.rows().len(), 1);
        assert_eq!(r.at(0), At::Nothing);
        for key in [Action::Activate, Action::Open, Action::Close] {
            assert!(
                matches!(r.on_action(key, 0), Flow::Continue),
                "{key:?} acted on a ruleset behind the error row"
            );
        }
        assert_eq!(r.open, vec![true, true]);
        assert!(!r.footer().contains("enter"), "{}", r.footer());
        r.error = None;

        // The empty shape still draws one row, and nothing walks to it.
        r.sets.clear();
        r.open.clear();
        assert_eq!(r.rows().len(), 1);
        assert_eq!(r.at(0), At::Nothing);
    }

    fn group_name(r: &Rules, g: usize) -> String {
        r.sets[g]
            .info
            .as_ref()
            .and_then(|i| i.name.clone())
            .unwrap_or_else(|| format!("ruleset {}", r.sets[g].id))
    }

    /// An empty answer is the normal state for a feature branch, so it has to
    /// say so rather than leave a blank screen that reads as a finding.
    #[test]
    fn no_rules_is_said_out_loud_and_told_apart_from_not_yet() {
        let mut r = screen();
        r.sets.clear();
        r.open.clear();

        r.answered = false;
        assert!(
            r.rows()[0].label.contains("asking"),
            "{:?}",
            r.rows()[0].label
        );

        r.answered = true;
        let row = &r.rows()[0];
        assert!(
            row.label.contains("no rules apply to main"),
            "{:?}",
            row.label
        );
        assert!(
            row.detail
                .as_deref()
                .unwrap_or("")
                .contains("may simply not target it"),
            "{:?}",
            row.detail
        );
        let pane = r.pane(0, 60, 40).lines.join("\n");
        assert!(pane.contains("not the same as unprotected"), "{pane}");
    }

    /// Measured, not assumed: `rules/branches/main/x` answers `[]` rather than
    /// 404 on a repo whose `main` has four rules, so the route takes the
    /// branch as the rest of the path and a slash must pass through. A `#` is
    /// legal in a branch name and must not.
    #[test]
    fn a_branch_name_keeps_its_slashes_and_loses_its_hashes() {
        assert_eq!(path_branch("feat/github-tui"), "feat/github-tui");
        assert_eq!(path_branch("fix#3"), "fix%233");
        assert_eq!(
            rules_req("roest1/dotfiles", "feat/x"),
            Req::Json {
                path: "repos/roest1/dotfiles/rules/branches/feat/x".into()
            }
        );
    }
}
