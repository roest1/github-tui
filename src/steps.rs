//! Jobs and steps of one run, as a tree.
//!
//! The fzf implementation listed these FLAT -- a job line, then its steps
//! indented under it, all in one unfilterable column -- because fzf has no
//! tree. `tui-core` does, and a matrix build is the case that makes the
//! difference: twelve jobs of fifteen steps is 180 rows flat, against twelve
//! rows with the one you care about opened.
//!
//! ## Which jobs open on arrival
//!
//! The failed ones, and only those. You reached this screen from a run whose
//! pane already showed the failure; the reason to come further is to see more
//! of it. A passing job's fifteen green steps are fifteen rows saying what its
//! own row said, and on a matrix build they bury the failure -- the same
//! argument that keeps the run body from expanding passing jobs, applied to a
//! tree instead of a transcript.
//!
//! If nothing failed, everything opens: a run you drilled into for curiosity
//! rather than for a failure should not present as a list of closed boxes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tui_core::{nav::Action, Flow, Pane, Row, Screen, Tick};

use crate::gh::{Fetcher, Got, Maybe, Req, IN_FLIGHT};
use crate::model::{Job, Run, Verdict};
use crate::paint::{DIM, OFF, ORG, RED};
use crate::runs::{all_completed, arrived, denied, log_answer, pager, refreshed, REFRESHING};
use crate::{logs, render, time};

const JOBS_EVERY: Duration = Duration::from_secs(5);
const PAGER_WAIT: Duration = Duration::from_secs(10);

/// What the cursor is on, resolved once.
///
/// The same device as `dots::console::At` and `root::At`, for the same reason:
/// `pane`, `on_action` and `footer` each need to know, and three independent
/// row walks is three chances to disagree about which row is which.
enum At {
    Job(usize),
    Step(usize, usize),
    Nothing,
}

pub struct Steps {
    repo: String,
    /// The run as the list last described it. Not the clone taken at `open`
    /// for good: that one said "in progress" for as long as the screen stayed
    /// up, and everything that asks whether the run is still going asked it.
    run: Run,
    /// The list `run` came from, which is the only thing that says when the
    /// run has finished. Asked again from here once its jobs have, because
    /// the screen underneath is not ticked while this one is on top of it.
    list: Req,
    list_seen: Option<Arc<Vec<Run>>>,
    multi: bool,
    fetch: Arc<Fetcher>,
    jobs: Vec<Job>,
    jobs_seen: Option<Arc<Vec<Job>>>,
    /// Why the jobs could not be fetched, when they could not. Neither
    /// "fetching" nor "no jobs yet", and drawn as neither.
    trouble: Option<String>,
    open: Vec<bool>,
    epoch_seen: u64,
    jobs_due: Option<Instant>,
    sel: usize,
    /// Where the cursor goes after this screen moved the rows under it. Read
    /// by `initial_sel` on the `Flow::Retarget` that follows, and cleared by
    /// the `focus` after that.
    land: Option<usize>,
    status: String,
}

impl Steps {
    /// `list` is the request `run` was read out of, so the run's own state
    /// can be re-read from the same place the caller reads it.
    pub fn open(repo: &str, run: Run, list: Req, multi: bool, fetch: Arc<Fetcher>) -> Self {
        let mut me = Self {
            repo: repo.to_string(),
            run,
            list,
            list_seen: None,
            multi,
            fetch,
            jobs: Vec::new(),
            jobs_seen: None,
            trouble: None,
            open: Vec::new(),
            epoch_seen: 0,
            jobs_due: None,
            sel: 0,
            land: None,
            status: String::new(),
        };
        // Already in hand whenever the parent screen's pane had it, which is
        // every time you arrive by pressing enter on a run you were looking at.
        me.fetch.request(me.jobs_req());
        me.sync();
        me
    }

    fn jobs_req(&self) -> Req {
        Req::Jobs {
            run: self.run.id,
            attempt: self.run.run_attempt,
            persist: self.run.verdict().done(),
        }
    }

    /// Pull whatever has landed into our own state, and re-arm the clock from
    /// it.
    ///
    /// The clock used to be re-armed from `self.run`, a clone taken when the
    /// screen opened and never touched again. Opened on a live run it said
    /// "live" forever, so the jobs were asked for every five seconds until
    /// the screen was closed -- long after the last of them had finished.
    ///
    /// Now each half is re-armed by its own arrival, and only by that:
    ///
    ///   * JOBS that have not all completed ask to be looked at again. Jobs
    ///     that have, stop the clock and ask the LIST once instead.
    ///   * the LIST then says whether the run is over. If it is, the run here
    ///     is replaced with the finished one and nothing is scheduled. If it
    ///     is not -- a `needs:` chain creates its later jobs only as the
    ///     earlier ones finish, so every job so far being done is not the run
    ///     being done -- the jobs clock starts again.
    ///
    /// "Its own arrival" is `arrived`, not the epoch: a log landing bumps the
    /// epoch too, and re-asking the list every time anything landed would
    /// have each answer trigger the next.
    fn sync(&mut self) {
        let now = Instant::now();
        if let Some(Got::Runs(runs)) = self.fetch.get(&self.list) {
            if arrived(&self.list_seen, &runs) {
                // The same ATTEMPT only. A rerun is the same id with new
                // jobs, and this screen was opened on one attempt's.
                let found = runs
                    .iter()
                    .find(|r| r.id == self.run.id && r.run_attempt == self.run.run_attempt);
                if let Some(run) = found {
                    self.run = run.clone();
                    let done = self.run.verdict().done();
                    self.jobs_due = (!done).then(|| now + JOBS_EVERY);
                    if done {
                        // Asked for as final now, which the fetcher answers by
                        // fetching once more and keeping it: the snapshot in
                        // hand was taken while the run was live.
                        self.fetch.request(self.jobs_req());
                    }
                }
                self.list_seen = Some(runs);
            }
        }
        match self.fetch.get(&self.jobs_req()) {
            Some(Got::Jobs(jobs)) => {
                self.trouble = None;
                if arrived(&self.jobs_seen, &jobs) {
                    self.take(&jobs);
                    if self.run.verdict().done() {
                        self.jobs_due = None;
                    } else if all_completed(&jobs) {
                        self.jobs_due = None;
                        self.fetch.refresh(self.list.clone());
                    } else {
                        self.jobs_due = Some(now + JOBS_EVERY);
                    }
                    self.jobs_seen = Some(jobs);
                    refreshed(&mut self.status);
                }
            }
            // Kept, and shown: a fetch that failed is not one still in
            // flight and is not a run with no jobs.
            Some(Got::Failed(e)) => {
                self.trouble = Some(e);
                refreshed(&mut self.status);
            }
            // A missing scope is not something asking again will change.
            Some(Got::Denied(d)) => {
                self.trouble = Some(denied(&d));
                self.jobs_due = None;
                refreshed(&mut self.status);
            }
            _ => {}
        }
        self.want_logs();
    }

    /// Replace the jobs with what arrived, keeping what was open.
    fn take(&mut self, jobs: &[Job]) {
        let first = self.jobs.is_empty();
        self.jobs = jobs.to_vec();
        // Opened on the FIRST arrival only. Recomputing it on every five-second
        // refresh would reopen a job you had just closed, which is the kind of
        // bug that reads as the keyboard not working.
        //
        // `sel` is NOT moved to the failed step here, though it used to be.
        // The cursor is `app`'s, and `app` asks where to put it once, at the
        // push; an arrival after that cannot move it. Setting `sel` anyway
        // left this screen believing the cursor was on the failed step while
        // it sat on row 0, so the footer described one row, `L` paged
        // another, and the log prefetched was for a job nobody was on.
        if first {
            let any_failed = self.jobs.iter().any(|j| j.verdict() == Verdict::Failed);
            self.open = self
                .jobs
                .iter()
                .map(|j| !any_failed || j.verdict() == Verdict::Failed)
                .collect();
        } else if self.open.len() != self.jobs.len() {
            // A run can grow jobs while it runs -- a `needs:` chain creates
            // them as its dependencies finish -- so the flags have to follow
            // the list rather than assume its length.
            self.open.resize(self.jobs.len(), false);
        }
    }

    /// The row a job's own line is drawn on.
    fn row_of(&self, job: usize) -> usize {
        self.jobs
            .iter()
            .enumerate()
            .take(job)
            .map(|(j, job)| {
                1 + if self.open.get(j).copied().unwrap_or(false) {
                    job.steps.len()
                } else {
                    0
                }
            })
            .sum()
    }

    /// Fetch the logs worth having: the failed jobs', plus whichever job the
    /// cursor is in. Both are completed-only, since GitHub serves nothing for a
    /// job still running.
    fn want_logs(&self) {
        for job in &self.jobs {
            if job.verdict() == Verdict::Failed && job.has_log() {
                self.fetch.request(Req::Log { job: job.id });
            }
        }
        if let Some(j) = self.job_under_cursor() {
            if self.jobs[j].has_log() {
                self.fetch.request(Req::Log {
                    job: self.jobs[j].id,
                });
            }
        }
    }

    fn job_under_cursor(&self) -> Option<usize> {
        match self.at(self.sel) {
            At::Job(j) | At::Step(j, _) => Some(j),
            At::Nothing => None,
        }
    }

    fn at(&self, sel: usize) -> At {
        let mut row = 0;
        for (j, job) in self.jobs.iter().enumerate() {
            if sel == row {
                return At::Job(j);
            }
            row += 1;
            if self.open.get(j).copied().unwrap_or(false) {
                for s in 0..job.steps.len() {
                    if sel == row {
                        return At::Step(j, s);
                    }
                    row += 1;
                }
            }
        }
        At::Nothing
    }

    /// The row to open on: the first failed step, else the first failed job,
    /// else the top.
    ///
    /// Asked of the screen rather than computed by the caller for the reason
    /// `Screen::initial_sel` exists -- the answer depends on which jobs this
    /// screen decided to open, and nothing outside it knows that.
    fn initial_sel(&self) -> usize {
        let mut row = 0;
        let mut first_failed_job = None;
        for (j, job) in self.jobs.iter().enumerate() {
            let job_row = row;
            if job.verdict() == Verdict::Failed && first_failed_job.is_none() {
                first_failed_job = Some(job_row);
            }
            row += 1;
            if self.open.get(j).copied().unwrap_or(false) {
                for step in &job.steps {
                    if step.verdict() == Verdict::Failed {
                        return row;
                    }
                    row += 1;
                }
            }
        }
        first_failed_job.unwrap_or(0)
    }

    fn log_of(&self, job: u64) -> Maybe<Arc<String>> {
        log_answer(self.fetch.get(&Req::Log { job }))
    }

    /// The log to show beside `job`: asked for only once the job has one.
    fn log_for(&self, job: &Job) -> Maybe<Arc<String>> {
        if job.has_log() {
            self.log_of(job.id)
        } else {
            Maybe::Waiting
        }
    }

    /// Fetch-and-wait, for the pager. See `Fetcher::block_on`.
    fn log_now(&mut self, job: u64) -> Option<Arc<String>> {
        self.fetch.block_on(&[Req::Log { job }], PAGER_WAIT);
        match self.log_of(job) {
            Maybe::Got(log) => Some(log),
            Maybe::Waiting => {
                self.status = "the log did not arrive".into();
                None
            }
            Maybe::Cannot(why) => {
                self.status = format!("could not fetch the log \u{2014} {why}");
                None
            }
        }
    }

    fn browse(&mut self, at: &At) -> Flow {
        let url = match at {
            At::Job(j) => self.jobs[*j]
                .html_url
                .clone()
                .unwrap_or_else(|| self.run.html_url.clone()),
            _ => self.run.html_url.clone(),
        };
        self.status = crate::browse(&url);
        Flow::Dirty
    }
}

impl Screen for Steps {
    fn title(&self) -> String {
        format!(
            "{} \u{b7} #{} \u{b7} {}",
            self.repo,
            self.run.run_number,
            logs::plain(&self.run.display_title)
        )
    }

    fn initial_sel(&self) -> usize {
        self.land.unwrap_or_else(|| Steps::initial_sel(self))
    }

    fn focus(&mut self, sel: usize) {
        self.sel = sel;
        self.land = None;
        self.status.clear();
        self.want_logs();
    }

    fn tick(&mut self) -> Tick {
        if self.fetch.landed(&mut self.epoch_seen) {
            self.sync();
            return Tick::Changed;
        }
        if self.fetch.pending() {
            return Tick::Wake(IN_FLIGHT);
        }
        let t = Instant::now();
        if self.jobs_due.is_some_and(|d| t >= d) {
            self.jobs_due = Some(t + JOBS_EVERY);
            self.fetch.refresh(self.jobs_req());
            return Tick::Wake(IN_FLIGHT);
        }
        match self.jobs_due.map(|d| d.saturating_duration_since(t)) {
            Some(d) => Tick::Wake(d),
            None => Tick::Idle,
        }
    }

    fn rows(&mut self) -> Vec<Row> {
        if self.jobs.is_empty() {
            if let Some(why) = &self.trouble {
                return vec![Row::leaf("could not list jobs").detail(logs::plain(why))];
            }
            return vec![Row::leaf(if self.fetch.pending() {
                "fetching jobs\u{2026}"
            } else {
                "no jobs yet \u{2014} waiting for a runner"
            })];
        }
        let mut rows = Vec::new();
        for (j, job) in self.jobs.iter().enumerate() {
            let (badge, color) = job.verdict().badge();
            // Names come from the workflow file, which on a pull request
            // from a fork is the fork's. Characters only: `logs::plain`.
            let mut r = Row::leaf(logs::plain(&job.name));
            r.group = true;
            r.expanded = self.open.get(j).copied().unwrap_or(false);
            rows.push(
                r.detail(job.elapsed().map(time::dur).unwrap_or_default())
                    .badged(badge, color),
            );
            if !self.open.get(j).copied().unwrap_or(false) {
                continue;
            }
            for step in &job.steps {
                let (badge, color) = step.verdict().badge();
                let mut r = Row::leaf(logs::plain(&step.name));
                r.depth = 1;
                rows.push(
                    r.detail(step.elapsed().map(time::dur).unwrap_or_default())
                        .badged(badge, color),
                );
            }
        }
        rows
    }

    fn pane(&mut self, sel: usize, cols: u16, rows: u16) -> Pane {
        let mut lines = render::run_header(&self.run, cols, self.multi);
        let room = rows.saturating_sub(lines.len() as u16);
        match self.at(sel) {
            At::Job(j) => {
                let job = self.jobs[j].clone();
                let log = self.log_for(&job);
                lines.extend(render::job_body(&job, log.view(|l| l.as_str()), cols, room));
            }
            At::Step(j, s) => {
                let job = self.jobs[j].clone();
                let step = job.steps[s].clone();
                let log = self.log_for(&job);
                lines.extend(render::step_body(
                    &job,
                    &step,
                    log.view(|l| l.as_str()),
                    cols,
                    room,
                ));
            }
            At::Nothing => match &self.trouble {
                Some(why) => {
                    lines.push(format!("{RED}could not list this run's jobs{OFF}"));
                    lines.push(format!("{DIM}{why}{OFF}"));
                }
                None => lines.push(format!("{DIM}nothing selected{OFF}")),
            },
        }
        // The jobs on screen are the last that arrived. When the refresh
        // after them failed, they are still worth showing -- with a line
        // saying they are no longer being kept up to date, and why.
        if let (Some(why), false) = (&self.trouble, self.jobs.is_empty()) {
            lines.push(String::new());
            lines.push(format!(
                "{RED}the last refresh failed{OFF} {DIM}\u{2014} {why}{OFF}"
            ));
        }
        if !self.status.is_empty() {
            lines.push(String::new());
            lines.push(format!("{ORG}{}{OFF}", self.status));
        }
        // Job and step names, a title and gh's own words are in these lines
        // beside this crate's colours, so they leave through the gate a log
        // line does.
        Pane {
            image: None,
            lines: lines.iter().map(|l| logs::clean(l)).collect(),
        }
    }

    fn on_action(&mut self, action: Action, sel: usize) -> Flow {
        self.status.clear();
        let at = self.at(sel);
        match (action, &at) {
            (Action::Activate, At::Job(j)) => {
                let j = *j;
                if let Some(flag) = self.open.get_mut(j) {
                    *flag = !*flag;
                }
                Flow::Dirty
            }
            // `l` opens a closed job and does nothing else. On an open one it
            // used to CLOSE it, being matched together with `enter` -- so the
            // key for "further in" went back out, and the pane beside an open
            // job could not be reached at all. Answering `Continue` there is
            // what lets `app` move the focus into the pane.
            (Action::Open, At::Job(j)) => match self.open.get_mut(*j) {
                Some(flag) if !*flag => {
                    *flag = true;
                    Flow::Dirty
                }
                _ => Flow::Continue,
            },
            (Action::Close, At::Job(j)) => {
                let j = *j;
                if let Some(flag) = self.open.get_mut(j) {
                    *flag = false;
                }
                Flow::Dirty
            }
            // On a step, `h` closes the job it belongs to -- the nvim thing to
            // do, and it beats making you move up to the parent row first.
            //
            // The step's row is gone once it does, so the cursor is sent to
            // the job's. Left where it was it kept its row NUMBER, which
            // after the steps above it vanished belongs to some later job.
            (Action::Close, At::Step(j, _)) => {
                let j = *j;
                if let Some(flag) = self.open.get_mut(j) {
                    *flag = false;
                }
                self.land = Some(self.row_of(j));
                Flow::Retarget
            }
            (Action::Key('o'), _) => self.browse(&at),
            (Action::Key('r'), _) => {
                self.fetch.refresh(self.jobs_req());
                // And the run itself, while it may still change: the list is
                // what restarts a clock that a failed fetch stopped.
                if !self.run.verdict().done() {
                    self.fetch.refresh(self.list.clone());
                }
                self.status = REFRESHING.into();
                Flow::Dirty
            }
            // One key, one meaning, in both screens: `L` pages whatever the
            // cursor is on. The pane already shows a step's whole log, so this
            // is for the ones too long to fit -- which is exactly when the run
            // body said "enter to browse" and you came here.
            (Action::Key('L'), At::Job(j)) => {
                let job = self.jobs[*j].clone();
                if !job.has_log() {
                    self.status = "no log until the job completes".into();
                    return Flow::Dirty;
                }
                let name = format!("job-{}", job.id);
                match self.log_now(job.id) {
                    Some(log) => {
                        let body = logs::fmt_log(log.lines()).join("\n");
                        match pager(&name, &body) {
                            Some(argv) => Flow::Detach(argv),
                            None => Flow::Dirty,
                        }
                    }
                    None => Flow::Dirty,
                }
            }
            (Action::Key('L'), At::Step(j, s)) => {
                let job = self.jobs[*j].clone();
                let step = job.steps[*s].clone();
                if !job.has_log() {
                    self.status = "no log until the job completes".into();
                    return Flow::Dirty;
                }
                let name = format!("step-{}-{}", job.id, step.number);
                match self.log_now(job.id) {
                    Some(log) => {
                        let slice = logs::step_slice(
                            &log,
                            step.started_at.as_deref().unwrap_or(""),
                            step.completed_at.as_deref().unwrap_or(""),
                            &step.name,
                        );
                        let body = logs::fmt_log(slice).join("\n");
                        match pager(&name, &body) {
                            Some(argv) => Flow::Detach(argv),
                            None => Flow::Dirty,
                        }
                    }
                    None => Flow::Dirty,
                }
            }
            _ => Flow::Continue,
        }
    }

    fn footer(&self) -> String {
        let what = match self.at(self.sel) {
            At::Job(_) => "enter open/close \u{b7} L whole job log",
            At::Step(_, _) => "L this step's log in a pager",
            At::Nothing => "j/k move",
        };
        format!("{what} \u{b7} o browser \u{b7} r refresh")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{JobList, RunList};

    fn run(concl: &str) -> Run {
        serde_json::from_str::<RunList>(&format!(
            r#"{{"workflow_runs":[{{"id":1,"display_title":"t","head_sha":"a","event":"push",
            "status":"completed","conclusion":"{concl}","html_url":"https://x",
            "created_at":"2026-08-25T19:30:00Z","run_number":1,"run_attempt":1}}]}}"#
        ))
        .expect("parses")
        .workflow_runs
        .remove(0)
    }

    const JOBS: &str = r#"{"jobs":[
      {"id":10,"name":"build","status":"completed","conclusion":"success",
       "started_at":"2026-08-25T19:30:02Z","completed_at":"2026-08-25T19:31:02Z",
       "steps":[{"number":1,"name":"checkout","status":"completed","conclusion":"success"},
                {"number":2,"name":"compile","status":"completed","conclusion":"success"}]},
      {"id":11,"name":"test","status":"completed","conclusion":"failure",
       "started_at":"2026-08-25T19:31:02Z","completed_at":"2026-08-25T19:35:02Z",
       "steps":[{"number":1,"name":"checkout","status":"completed","conclusion":"success"},
                {"number":2,"name":"cargo test","status":"completed","conclusion":"failure"}]}
    ]}"#;

    /// Built without a fetcher: these tests are about the tree, and a screen
    /// that needed the network to be walked would not be testable at all.
    fn screen(concl: &str, jobs_json: &str) -> Steps {
        let jobs = serde_json::from_str::<JobList>(jobs_json)
            .expect("parses")
            .jobs;
        let any_failed = jobs.iter().any(|j| j.verdict() == Verdict::Failed);
        let open = jobs
            .iter()
            .map(|j| !any_failed || j.verdict() == Verdict::Failed)
            .collect();
        Steps {
            repo: "roest1/dotfiles".into(),
            run: run(concl),
            list: list(),
            list_seen: None,
            multi: false,
            fetch: Arc::new(Fetcher::new("roest1/dotfiles")),
            jobs,
            jobs_seen: None,
            trouble: None,
            open,
            epoch_seen: 0,
            jobs_due: None,
            sel: 0,
            land: None,
            status: String::new(),
        }
    }

    /// Stands in for the run list. Asking for it again runs `gh` with a flag
    /// it refuses, so a test that gets as far as re-asking stays on this
    /// machine.
    fn list() -> Req {
        Req::Cmd {
            args: vec!["--definitely-not-a-gh-flag".into()],
        }
    }

    fn parsed(jobs_json: &str) -> Got {
        Got::Jobs(Arc::new(
            serde_json::from_str::<JobList>(jobs_json)
                .expect("parses")
                .jobs,
        ))
    }

    /// A screen opened on a run still going, before any of its jobs landed,
    /// with both jobs' logs already in hand so nothing here asks for one.
    fn live() -> Steps {
        let mut s = screen("success", r#"{"jobs":[]}"#);
        s.run.status = Some("in_progress".into());
        s.run.conclusion = None;
        for job in [10, 11] {
            s.fetch
                .seed(Req::Log { job }, Got::Log(Arc::new(String::new())));
        }
        s
    }

    fn text(pane: Pane) -> String {
        pane.lines.join("\n")
    }

    /// The invariant every tree screen in this repo carries: each selectable
    /// index resolves to the row actually drawn at it. Jobs collapse, so the
    /// mapping moves under it.
    fn walk_agrees(s: &mut Steps) {
        let rows = s.rows();
        for (i, row) in rows.iter().enumerate() {
            match s.at(i) {
                At::Job(j) => assert_eq!(row.label, s.jobs[j].name, "row {i}"),
                At::Step(j, k) => assert_eq!(row.label, s.jobs[j].steps[k].name, "row {i}"),
                At::Nothing => panic!("selectable row {i} ({}) resolves to nothing", row.label),
            }
        }
    }

    #[test]
    fn the_walk_matches_the_rows_in_every_shape() {
        let mut s = screen("failure", JOBS);
        walk_agrees(&mut s);
        s.open = vec![true, true];
        walk_agrees(&mut s);
        s.open = vec![false, false];
        walk_agrees(&mut s);
        s.open = vec![true, false];
        walk_agrees(&mut s);
    }

    /// You came here from a failure. The passing job's steps are rows that say
    /// what its own row said; on a matrix build they bury the one that matters.
    #[test]
    fn only_the_failed_job_opens_when_something_failed() {
        let s = screen("failure", JOBS);
        assert_eq!(s.open, vec![false, true]);
    }

    /// And a run opened out of curiosity should not be a list of closed boxes.
    #[test]
    fn everything_opens_when_nothing_failed() {
        let all_green = JOBS.replace("\"failure\"", "\"success\"");
        let s = screen("success", &all_green);
        assert_eq!(s.open, vec![true, true]);
    }

    /// The cursor lands on the failed STEP, not on the job holding it -- the
    /// reason you drilled in is one row further down.
    #[test]
    fn it_opens_on_the_failed_step() {
        let mut s = screen("failure", JOBS);
        let want = Steps::initial_sel(&s);
        let rows = s.rows();
        assert_eq!(rows[want].label, "cargo test");
    }

    /// A `needs:` chain creates jobs as its dependencies finish, so the flags
    /// have to follow the list rather than assume its length -- indexing
    /// `open` by a job that arrived later would panic mid-refresh.
    #[test]
    fn a_run_that_grows_jobs_does_not_lose_its_open_flags() {
        let mut s = screen("failure", JOBS);
        s.open = vec![true];
        walk_agrees(&mut s);
        s.open = vec![true, true, true, true];
        walk_agrees(&mut s);
    }

    #[test]
    fn a_run_with_no_jobs_says_so_rather_than_drawing_nothing() {
        let mut s = screen("success", r#"{"jobs":[]}"#);
        let rows = s.rows();
        assert_eq!(rows.len(), 1);
        assert!(matches!(s.at(0), At::Nothing));
    }

    /// The clock is re-armed by what ARRIVED. It used to be re-armed from the
    /// run as it was when the screen opened, which never stopped being live:
    /// a run watched to its finish was polled every five seconds for good.
    #[test]
    fn a_run_watched_to_its_finish_stops_being_polled() {
        let mut s = live();
        let going = JOBS.replace(
            r#""status":"completed","conclusion":"failure""#,
            r#""status":"in_progress""#,
        );
        s.fetch.seed(s.jobs_req(), parsed(&going));
        s.sync();
        assert!(s.jobs_due.is_some(), "stopped watching jobs still going");
        assert!(
            !s.fetch.pending(),
            "asked for something with jobs still going"
        );

        // Every job has completed. Nothing more can come of asking for them
        // again, so the clock stops -- and the LIST is asked, because only it
        // says whether the run is over.
        s.fetch.seed(s.jobs_req(), parsed(JOBS));
        s.sync();
        assert!(
            s.jobs_due.is_none(),
            "still polling jobs that have all finished"
        );
        assert!(
            s.fetch.pending(),
            "did not ask the list whether the run is over"
        );
        assert!(
            !s.run.verdict().done(),
            "called the run finished on its own say-so"
        );

        // The list says it is still going: a `needs:` chain with jobs it has
        // not created yet. Watching starts again.
        let mut still = s.run.clone();
        still.updated_at = Some("2026-08-25T19:36:00Z".into());
        s.fetch.seed(list(), Got::Runs(Arc::new(vec![still])));
        s.sync();
        assert!(s.jobs_due.is_some(), "gave up on a run the list calls live");

        // And once the list says it is over, the run here is the finished
        // one and nothing is scheduled.
        let mut over = s.run.clone();
        over.status = Some("completed".into());
        over.conclusion = Some("failure".into());
        s.fetch.seed(list(), Got::Runs(Arc::new(vec![over])));
        s.sync();
        assert!(
            s.run.verdict().done(),
            "the run was not updated from the list"
        );
        assert!(s.jobs_due.is_none(), "still polling a finished run");
    }

    /// A list that mentions a LATER attempt of this run is about other jobs.
    /// This screen keeps the attempt it was opened on.
    #[test]
    fn a_rerun_in_the_list_is_not_this_run() {
        let mut s = live();
        let mut rerun = s.run.clone();
        rerun.run_attempt = 2;
        rerun.status = Some("completed".into());
        rerun.conclusion = Some("success".into());
        s.fetch.seed(list(), Got::Runs(Arc::new(vec![rerun])));
        s.sync();
        assert_eq!(s.run.run_attempt, 1);
        assert!(!s.run.verdict().done());
    }

    /// A fetch that failed is one row SAYING so, and that row is nothing: no
    /// job is behind it for a key to act on. It used to read "no jobs yet --
    /// waiting for a runner", about a run that had finished.
    #[test]
    fn jobs_that_could_not_be_fetched_are_a_row_saying_so() {
        for got in [
            Got::Failed("gh: Not Found (HTTP 404)".into()),
            Got::Denied(crate::scopes::Denied {
                accepted: vec!["repo".into()],
                suggest: "repo".into(),
            }),
        ] {
            let mut s = screen("failure", r#"{"jobs":[]}"#);
            s.fetch.seed(s.jobs_req(), got);
            s.sync();
            let rows = s.rows();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].label, "could not list jobs");
            let why = rows[0].detail.clone().unwrap_or_default();
            assert!(
                why.contains("404") || why.contains("gh auth refresh"),
                "{why}"
            );
            assert!(matches!(s.at(0), At::Nothing));
            assert!(matches!(s.on_action(Action::Activate, 0), Flow::Continue));
            let pane = text(s.pane(0, 60, 40));
            assert!(pane.contains("could not list this run's jobs"), "{pane}");
            assert!(
                !pane.contains("fetching") && !pane.contains("waiting"),
                "{pane}"
            );
        }
    }

    /// When a REFRESH fails the jobs already on screen stay, since they are
    /// the last thing known -- under a line saying they have stopped being
    /// kept up to date.
    #[test]
    fn a_failed_refresh_keeps_the_jobs_and_says_it_failed() {
        let mut s = live();
        s.jobs = serde_json::from_str::<JobList>(JOBS).expect("parses").jobs;
        s.open = vec![true, true];
        s.fetch
            .seed(s.jobs_req(), Got::Failed("gh: connection reset".into()));
        s.sync();
        assert_eq!(s.rows().len(), 6, "dropped the jobs it had");
        let pane = text(s.pane(0, 60, 40));
        assert!(pane.contains("the last refresh failed"), "{pane}");
        assert!(pane.contains("connection reset"), "{pane}");
    }

    /// Jobs that land after the screen opened do not move `sel`. The cursor
    /// is `app`'s and stays on row 0, so a `sel` moved to the failed step had
    /// the footer and the log prefetch describing a row nobody was on.
    #[test]
    fn a_late_arrival_leaves_the_cursor_where_app_has_it() {
        let mut s = live();
        s.run = run("failure");
        s.fetch.seed(s.jobs_req(), parsed(JOBS));
        s.sync();
        assert_eq!(s.open, vec![false, true], "did not open the failure");
        assert_eq!(s.sel, 0, "moved its own idea of the cursor");
        assert!(s.footer().contains("enter open/close"), "{}", s.footer());
        // What `app` is told if it asks -- which it does on a push.
        assert_eq!(s.rows()[Screen::initial_sel(&s)].label, "cargo test");
    }

    /// `l` goes further in: it opens a closed job, and on an open one it
    /// answers `Continue`, which is what moves the focus into the pane. It
    /// never closes anything. `enter` is the toggle.
    #[test]
    fn l_opens_a_job_and_never_closes_one() {
        let mut s = screen("failure", JOBS);
        assert_eq!(s.open, vec![false, true]);
        assert!(matches!(s.on_action(Action::Open, 0), Flow::Dirty));
        assert_eq!(s.open, vec![true, true]);
        assert!(matches!(s.on_action(Action::Open, 0), Flow::Continue));
        assert_eq!(s.open, vec![true, true], "l closed an open job");
        // A step has nothing to open either.
        assert!(matches!(s.on_action(Action::Open, 1), Flow::Continue));

        assert!(matches!(s.on_action(Action::Activate, 0), Flow::Dirty));
        assert_eq!(s.open, vec![false, true]);
        assert!(matches!(s.on_action(Action::Activate, 0), Flow::Dirty));
        assert_eq!(s.open, vec![true, true]);
    }

    /// `h` on a step closes its job, and the cursor goes to the job's row.
    /// Its own row is gone, and the same row NUMBER is now some later job.
    #[test]
    fn closing_a_job_from_a_step_puts_the_cursor_on_that_job() {
        let mut s = screen("failure", JOBS);
        s.open = vec![true, true];
        // build, its two steps, test, checkout, [cargo test]
        assert_eq!(s.rows()[5].label, "cargo test");
        assert!(matches!(s.on_action(Action::Close, 5), Flow::Retarget));
        assert_eq!(s.open, vec![true, false]);
        let at = Screen::initial_sel(&s);
        assert_eq!(s.rows()[at].label, "test");
        // Once `app` has put the cursor there, the screen opens where it
        // always did.
        s.focus(at);
        assert!(s.land.is_none());
    }

    /// Names come from the workflow file. Whatever is in one, a row and a
    /// pane line hold characters and this crate's own colours -- nothing a
    /// terminal would act on.
    #[test]
    fn a_job_name_cannot_carry_an_escape_to_the_terminal() {
        let mut s = screen("failure", JOBS);
        s.jobs[1].name = "test\u{1b}]52;c;QQ==\u{7}\u{1b}[2J".into();
        s.jobs[1].steps[1].name = "cargo\u{1b}[H test\r".into();
        s.run.display_title = "title\u{1b}]0;x\u{7}".into();
        for row in s.rows() {
            assert!(!row.label.chars().any(char::is_control), "{:?}", row.label);
        }
        assert!(!s.title().chars().any(char::is_control), "{:?}", s.title());
        for sel in 0..s.rows().len() {
            for line in s.pane(sel, 60, 40).lines {
                assert!(
                    !line.contains("52;c") && !line.contains("[2J") && !line.contains('\r'),
                    "{line:?}"
                );
            }
        }
    }
}
