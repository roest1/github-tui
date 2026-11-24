//! The Actions screen: workflow runs, newest first, with the failure opened up.
//!
//! The screen the port was sequenced around. It is the only one that exercises
//! live refresh, cross-session caching and the step-log slicing at the same
//! time, which is why it came second rather than last -- if this shape survives
//! a CI dashboard, the remaining screens are lists with previews.
//!
//! ## The two clocks
//!
//! A run list goes stale; a run in flight changes. Those are different
//! intervals and the screen runs both:
//!
//!   * the LIST is re-asked every 15s, but only while something on it is still
//!     going. A list of finished runs cannot change except by someone pushing,
//!     and `r` is there for that.
//!   * the SELECTED run's jobs are re-asked every 5s, again only while it is
//!     live. That is what makes a step flip from queued to running to green
//!     under the cursor without a keystroke.
//!
//! Both stop by themselves the moment the thing they are watching finishes,
//! which is the property the fzf implementation got from a preview rescheduling
//! its own tick, and the reason neither needs a stop button.
//!
//! They are reported through `Tick::Wake`, NOT `Tick::Busy`. Busy sets the poll
//! timeout to one frame, so a nine-minute run would have spun the process at
//! 60fps for nine minutes to notice something that can only arrive on a
//! five-second boundary.
//!
//! ## What the list column says, and what it does not
//!
//! The fzf list had ~100 columns and spent them: glyph, HEAD marker, age,
//! duration, event, branch, workflow, title. `tui-core`'s left column is 52, so
//! the row has to choose, and the triple that IDENTIFIES a run is title, age
//! and verdict. Branch, event, workflow, duration, actor and run number all
//! moved into the pane, which is wider than the old preview was and is on
//! screen at the same time.
//!
//! The branch comes back into the row when -- and only when -- you are looking
//! across branches, because that is when it is the thing that distinguishes two
//! otherwise identical rows. Same rule as the root screen's slug.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tui_core::{nav::Action, Flow, Pane, Row, Screen, Tick};

use crate::gh::{self, Fetcher, Got, Maybe, Req, IN_FLIGHT};
use crate::index::Repo;
use crate::model::{Job, Run};
use crate::paint::{DIM, OFF, ORG};
use crate::scopes::Denied;
use crate::steps::Steps;
use crate::{logs, render, time};

const LIST_EVERY: Duration = Duration::from_secs(15);
const JOBS_EVERY: Duration = Duration::from_secs(5);
/// How long paging a log may block. Long enough for a cold 15KB fetch on a bad
/// connection, short enough that a hung network does not feel like a crash.
const PAGER_WAIT: Duration = Duration::from_secs(10);

/// Which runs the list is showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Everything the repo has run, which is the right default when you have
    /// just arrived and do not yet know what you are looking for.
    All,
    /// One branch -- normally the one checked out here.
    Branch(String),
    /// One commit -- the pull request menu's "Workflow runs for this commit".
    Commit(String),
}

impl Scope {
    fn query(&self) -> String {
        let mut q = "per_page=40".to_string();
        match self {
            Scope::All => {}
            Scope::Branch(b) => q.push_str(&format!("&branch={}", urlencode(b))),
            Scope::Commit(sha) => q.push_str(&format!("&head_sha={sha}")),
        }
        q
    }

    fn label(&self) -> String {
        match self {
            Scope::All => "all branches".into(),
            Scope::Branch(b) => format!("\u{2387} {b}"),
            Scope::Commit(sha) => format!("commit {}", short(sha)),
        }
    }
}

/// A commit, at the length everything here says it at.
///
/// Seven characters, which is what `git log --oneline` and github.com both
/// show. Cut on a CHARACTER boundary rather than a byte one: a sha is hex and
/// could not break, but this is handed whatever the API returned.
pub(crate) fn short(sha: &str) -> &str {
    match sha.char_indices().nth(7) {
        Some((i, _)) => &sha[..i],
        None => sha,
    }
}

/// Percent-encode a branch name for a query string.
///
/// Branch names legally contain `/`, `#` and `+`, all of which change the
/// meaning of a query parameter -- `feat/x+y` would otherwise ask for the
/// branch `feat/x y`. Only unreserved characters pass through.
pub(crate) fn urlencode(s: &str) -> String {
    gh::percent(s, b"")
}

/// What `r` says while its answer is on the way, kept as a constant so the
/// arrival can recognise the line and take it down. Left up, the pane went on
/// saying "refreshing" under a list that had already been refreshed.
pub(crate) const REFRESHING: &str = "refreshing\u{2026}";

/// The answer to `r` is in: take down the line that said it was coming.
pub(crate) fn refreshed(status: &mut String) {
    if status == REFRESHING {
        status.clear();
    }
}

/// Is `got` an answer this screen has not read yet?
///
/// The fetcher hands out each answer behind an `Arc` and makes a new one per
/// arrival, so two reads of the same arrival are the same pointer. That is
/// what tells "the list came back" from "something else came back and the
/// list is still the one from before" -- a difference `Fetcher::epoch` cannot
/// see, because every arrival moves it.
pub(crate) fn arrived<T>(seen: &Option<Arc<T>>, got: &Arc<T>) -> bool {
    !seen.as_ref().is_some_and(|s| Arc::ptr_eq(s, got))
}

/// A refusal, as the line that changes it. See `scopes`.
pub(crate) fn denied(d: &Denied) -> String {
    format!("this token lacks the scope \u{2014} {}", d.fix())
}

/// An answer in the three states a fetch has. `pick` takes the payload out
/// of the one variant that carries it; any other is still on its way.
fn answer<T>(got: Option<Got>, pick: impl FnOnce(Got) -> Option<T>) -> Maybe<T> {
    match got {
        Some(Got::Denied(d)) => Maybe::Cannot(denied(&d)),
        Some(Got::Failed(e)) => Maybe::Cannot(e),
        Some(got) => pick(got).map_or(Maybe::Waiting, Maybe::Got),
        None => Maybe::Waiting,
    }
}

/// One run's jobs.
fn jobs_answer(got: Option<Got>) -> Maybe<Arc<Vec<Job>>> {
    answer(got, |got| match got {
        Got::Jobs(j) => Some(j),
        _ => None,
    })
}

/// One job's log. Shared, not copied: it can be megabytes, and every caller
/// only reads it.
pub(crate) fn log_answer(got: Option<Got>) -> Maybe<Arc<String>> {
    answer(got, |got| match got {
        Got::Log(l) => Some(l),
        _ => None,
    })
}

/// Have these jobs all finished? An empty list has not: it is a run still
/// waiting for a runner.
pub(crate) fn all_completed(jobs: &[Job]) -> bool {
    !jobs.is_empty() && jobs.iter().all(Job::has_log)
}

/// A destructive action waiting on one more keystroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Confirm {
    Rerun,
    RerunFailed,
    Cancel,
}

/// A confirm in force: what was asked, and of WHICH run.
///
/// The run is held by id, not by where the cursor was. The list is replaced
/// under the cursor every time a refresh lands, and a push puts a new run in
/// row 0 -- so a prompt that remembered only its verb said "rerun #41" and
/// then, on `y`, reran whatever had moved into that row since.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pending {
    what: Confirm,
    id: u64,
    number: u64,
}

impl Pending {
    /// The `gh` arguments that do it.
    ///
    /// `--repo` on every one. `github` is launched from anywhere, and without
    /// it `gh run` resolves the repository from the working directory: an
    /// error outside a clone, and inside a DIFFERENT clone a rerun or a
    /// cancel sent to that repository's run of the same id.
    fn argv(&self, repo: &str) -> Vec<String> {
        let verb = match self.what {
            Confirm::Cancel => "cancel",
            Confirm::Rerun | Confirm::RerunFailed => "rerun",
        };
        let mut argv = vec![
            "run".to_string(),
            verb.to_string(),
            self.id.to_string(),
            "--repo".to_string(),
            repo.to_string(),
        ];
        if self.what == Confirm::RerunFailed {
            argv.push("--failed".to_string());
        }
        argv
    }
}

pub struct Actions {
    repo: String,
    scope: Scope,
    /// The branch checked out in the clone, so `b` has something to toggle back
    /// to. `None` for a detached HEAD or an unborn branch, which is why the
    /// toggle has to cope with there being no branch scope at all.
    branch: Option<String>,
    /// The commit whose runs get the marker: `HEAD` on a screen opened from a
    /// clone, and the pull request's head on one opened from its menu -- where
    /// it answers "which of this branch's runs are the ones I came from" the
    /// moment `b` widens past the commit.
    head_sha: Option<String>,
    /// The commit this screen was OPENED for, if it was opened for one.
    ///
    /// Not the same question as `scope`, which is where `b` has got to since.
    /// Remembering it is what makes `b` reversible on a screen the pull
    /// request menu opened: the pair offered there is this commit and the
    /// branch it is on, because widening to every branch is a door that does
    /// not come back -- nothing on screen afterwards names the commit again.
    commit: Option<String>,
    /// Shared with the jobs & steps screen pushed on top, so drilling into
    /// a run reuses the jobs and logs already fetched for its pane instead of
    /// asking for them again one keystroke later.
    fetch: Arc<Fetcher>,
    runs: Vec<Run>,
    /// The list answer `runs` was read from, to tell its next arrival from
    /// everything else that lands. See `arrived`.
    list_seen: Option<Arc<Vec<Run>>>,
    /// True when the repo has more than one workflow, which is the only time
    /// naming the workflow distinguishes anything.
    multi: bool,
    error: Option<String>,
    status: String,
    epoch_seen: u64,
    list_due: Option<Instant>,
    jobs_due: Option<Instant>,
    sel: usize,
    pending: Option<Pending>,
    /// What `y` has sent and GitHub has not answered yet. See `act`.
    sent: Vec<Pending>,
}

impl Actions {
    /// Open on a repo. Costs one `git rev-parse` and no network.
    ///
    /// The git spawn is the one place this crate shells out where `index` reads
    /// a file instead, and the asymmetry is the point: `index` resolves 27
    /// repos at startup, where this resolves one, after the first frame is
    /// already drawn. Reimplementing packed-ref lookup to save 3ms behind a
    /// network request would be the wrong trade.
    pub fn open(repo: &Repo) -> Option<Self> {
        let slug = match &repo.remote {
            crate::index::Remote::GitHub { owner, name } => format!("{owner}/{name}"),
            _ => return None,
        };
        let head_sha = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo.path)
            .args(["rev-parse", "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty());
        let branch = repo
            .branch
            .clone()
            .filter(|b| !b.starts_with("detached at "));
        let me = Self {
            fetch: Arc::new(Fetcher::new(&slug)),
            repo: slug,
            // All branches on arrival. A repo you have just opened is one you
            // want the state of, and scoping to the branch you happen to be
            // standing on hides the run that is actually failing.
            scope: Scope::All,
            branch,
            head_sha,
            commit: None,
            runs: Vec::new(),
            list_seen: None,
            multi: false,
            error: None,
            status: String::new(),
            epoch_seen: 0,
            list_due: None,
            jobs_due: None,
            sel: 0,
            pending: None,
            sent: Vec::new(),
        };
        me.fetch.request(me.list_req());
        Some(me)
    }

    /// Open on one commit, sharing the repo's fetcher.
    ///
    /// The pull request menu's route in. It takes a slug and a sha rather than
    /// an `index::Repo` because a pull request's head commit need not be
    /// anywhere on this disk -- it is a fork, or a branch this clone has never
    /// fetched -- so there is no `git rev-parse` to do and nothing local to
    /// read. That is the whole reason this constructor had to exist.
    ///
    /// The fetcher is the pull request screen's own, which is the same repo. A
    /// run LIST is a question about now and is never cached, but a completed
    /// run's jobs and logs are, so drilling into a run from here reuses
    /// whatever the Actions tab has already fetched.
    pub fn for_commit(repo: &str, sha: &str, branch: Option<&str>, fetch: Arc<Fetcher>) -> Self {
        let me = Self {
            fetch,
            repo: repo.to_string(),
            scope: Scope::Commit(sha.to_string()),
            branch: branch.map(str::to_string),
            // Marking the commit is what tells you, once `b` has widened to
            // the branch, which of its runs belong to the one you came from.
            head_sha: Some(sha.to_string()),
            commit: Some(sha.to_string()),
            runs: Vec::new(),
            list_seen: None,
            multi: false,
            error: None,
            status: String::new(),
            epoch_seen: 0,
            list_due: None,
            jobs_due: None,
            sel: 0,
            pending: None,
            sent: Vec::new(),
        };
        me.fetch.request(me.list_req());
        me
    }

    /// The shared fetcher, so the repo's other screens reuse what this one has
    /// already asked for -- the runs list is the Branches screen's checks
    /// column, so sharing it turns a second call into none.
    pub fn fetcher(&self) -> Arc<Fetcher> {
        Arc::clone(&self.fetch)
    }

    fn list_req(&self) -> Req {
        Req::Runs {
            query: self.scope.query(),
        }
    }

    fn jobs_req(&self, run: &Run) -> Req {
        Req::Jobs {
            run: run.id,
            attempt: run.run_attempt,
            // The list already told us whether this run is finished, so no
            // second call is needed to decide whether the answer may be
            // cached. See the note on `Req::Jobs`.
            persist: run.verdict().done(),
        }
    }

    /// The run drawn at row `sel`.
    ///
    /// None of them while the list could not be read: `rows()` then draws ONE
    /// row saying so, and the runs from before the failure are still held
    /// here underneath it. Resolving row 0 to the first of those let `e` and
    /// then `y` rerun a run that was not on screen, behind an error pane that
    /// never showed the prompt. This is the screen's row walk, so every key
    /// and both clocks get the same answer `rows()` drew.
    fn run(&self, sel: usize) -> Option<&Run> {
        if self.error.is_some() {
            return None;
        }
        self.runs.get(sel)
    }

    /// Whether the selected run's jobs are worth asking for again.
    ///
    /// A run still going, whose jobs in hand have not all finished. Once they
    /// have, another poll can only return the same thing -- unless the run
    /// has more jobs to create, and the list is what knows that. See `sync`.
    fn watching(&self, sel: usize) -> bool {
        self.run(sel).is_some_and(|run| {
            !run.verdict().done()
                && !matches!(self.jobs_of(run), Maybe::Got(jobs) if all_completed(&jobs))
        })
    }

    /// Pull whatever has landed into our own state.
    fn sync(&mut self) {
        self.hear_back();
        let now = Instant::now();
        // Did the LIST come back, as opposed to anything else? This runs on
        // every arrival, and the answer in the map is the same list until its
        // own refresh lands.
        let mut listed = false;
        match self.fetch.get(&self.list_req()) {
            Some(Got::Runs(runs)) => {
                if arrived(&self.list_seen, &runs) {
                    listed = true;
                    self.runs = runs.as_ref().clone();
                    let mut names: Vec<&str> =
                        self.runs.iter().filter_map(|r| r.name.as_deref()).collect();
                    names.sort_unstable();
                    names.dedup();
                    self.multi = names.len() > 1;
                    // Re-armed from what arrived rather than on a fixed
                    // timer: a list of finished runs schedules nothing.
                    //
                    // And only when the list ITSELF arrived. Done on every
                    // arrival, the five-second jobs poll of a live run pushed
                    // this deadline fifteen seconds out three times before it
                    // could fall due, so the list was never asked again: the
                    // run never showed as finished, and so its jobs were
                    // polled for as long as the screen stayed open.
                    self.list_due = self
                        .runs
                        .iter()
                        .any(|r| !r.verdict().done())
                        .then(|| now + LIST_EVERY);
                    self.list_seen = Some(runs);
                    refreshed(&mut self.status);
                }
                self.error = None;
            }
            Some(Got::Failed(e)) => {
                self.error = Some(e);
                refreshed(&mut self.status);
            }
            // Refused for want of a scope is an answer, and "no workflow runs
            // here yet" is not what it says. Asking again every fifteen
            // seconds would not change it either; `gh auth refresh` does.
            Some(Got::Denied(d)) => {
                self.error = Some(denied(&d));
                self.list_due = None;
                refreshed(&mut self.status);
            }
            _ => {}
        }
        // Whatever the cursor is on drives the jobs clock, and what arrived is
        // what re-arms it: a live run whose jobs have all completed is not
        // asked again.
        //
        // A list that has just arrived still calling the run live overrules
        // that once. Jobs in a `needs:` chain do not exist until what they
        // wait on has finished, so "every job has completed" can be true of a
        // run with half its work ahead of it -- and the list is the only
        // thing here that says which.
        let live = self.run(self.sel).is_some_and(|r| !r.verdict().done());
        self.jobs_due = (self.watching(self.sel) || (listed && live)).then(|| now + JOBS_EVERY);
        self.want_jobs(self.sel);
    }

    /// Ask for the jobs of the run at `sel` and of the one after it, plus the
    /// logs of any failed jobs in them.
    ///
    /// Driven by `focus`, not by `pane`, which is the rule this repo already
    /// paid for in the font browser: a row you skim PAST still has to be
    /// fetched, because that is precisely what makes the next keypress instant
    /// rather than the current one slow.
    ///
    /// One neighbour, not a window. The font browser prefetches several because
    /// a download is slow and a catalogue is browsed slowly; here each row is a
    /// `gh api` call, and scrolling a forty-run list with a window of five
    /// would fire a hundred requests to answer the one you stopped on. Down is
    /// the direction of travel, so the row below is the one worth having.
    fn want_jobs(&self, sel: usize) {
        for i in [sel, sel + 1] {
            let Some(run) = self.run(i) else { continue };
            let req = self.jobs_req(run);
            self.fetch.request(req.clone());
            let Some(Got::Jobs(jobs)) = self.fetch.get(&req) else {
                continue;
            };
            for job in jobs.iter() {
                // Only failed jobs, and only completed ones. GitHub serves
                // nothing for a job still running, so asking is a wasted round
                // trip that comes back looking like an empty log.
                if job.verdict() == crate::model::Verdict::Failed && job.has_log() {
                    self.fetch.request(Req::Log { job: job.id });
                }
            }
        }
    }

    fn jobs_of(&self, run: &Run) -> Maybe<Arc<Vec<Job>>> {
        jobs_answer(self.fetch.get(&self.jobs_req(run)))
    }

    fn log_of(&self, job: u64) -> Maybe<Arc<String>> {
        log_answer(self.fetch.get(&Req::Log { job }))
    }

    /// Where `b` goes from here, or `None` when it has nowhere to go.
    ///
    /// Which two scopes `b` moves between depends on which question the
    /// screen was opened to answer. Opened on a repo it is every branch or
    /// the one checked out here; opened on a commit it is that commit or the
    /// branch carrying it, because All would be a one-way door.
    ///
    /// Asked by the key AND by the footer that names its result. They were
    /// two matches over the same three fields and had drifted: on a commit
    /// with no branch the footer promised "just <sha>" for a key that widened
    /// to every branch, and on a detached HEAD it promised "all branches" for
    /// a key that did nothing.
    fn next_scope(&self) -> Option<Scope> {
        match (&self.scope, &self.branch, &self.commit) {
            (Scope::Commit(_), Some(b), _) => Some(Scope::Branch(b.clone())),
            (Scope::Commit(_), None, _) => Some(Scope::All),
            (_, _, Some(sha)) => Some(Scope::Commit(sha.clone())),
            (Scope::All, Some(b), None) => Some(Scope::Branch(b.clone())),
            (Scope::Branch(_), _, None) => Some(Scope::All),
            (Scope::All, None, None) => None,
        }
    }

    fn toggle_scope(&mut self) -> Flow {
        let Some(next) = self.next_scope() else {
            self.status = "no branch checked out here to scope to".into();
            return Flow::Dirty;
        };
        self.scope = next;
        // The runs on screen belong to the old scope. Clearing them is what
        // stops the list showing one scope while its label claims another --
        // and an error belongs to the scope that failed, likewise.
        self.runs.clear();
        self.list_seen = None;
        self.error = None;
        self.sel = 0;
        self.list_due = None;
        self.fetch.request(self.list_req());
        // Read whatever is already on hand for the new scope, rather than
        // waiting for an arrival that is not coming. `tick` syncs on the
        // fetcher's EPOCH moving, and toggling back to a scope answered a
        // moment ago moves nothing -- so without this the screen sat on an
        // empty list saying "no workflow runs here yet" about a scope that
        // had six in it. It hid behind the clocks: a live run re-arms the
        // list timer and that refresh does bump the epoch, so it only showed
        // on a scope where everything had finished.
        self.sync();
        // Every row was replaced, so the cursor goes where this screen opens
        // -- the newest run. `Dirty` left it at whatever row number it had
        // been on in the OTHER scope, while `sel` here said 0: the pane, the
        // jobs clock and the prefetch followed one run and the keys another.
        Flow::Retarget
    }

    fn refresh(&mut self) -> Flow {
        self.fetch.refresh(self.list_req());
        if let Some(run) = self.run(self.sel) {
            self.fetch.refresh(self.jobs_req(run));
        }
        self.status = REFRESHING.into();
        Flow::Dirty
    }

    fn browse(&mut self, sel: usize) -> Flow {
        let Some(run) = self.run(sel) else {
            return Flow::Continue;
        };
        let url = run.html_url.clone();
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        match std::process::Command::new(opener).arg(&url).spawn() {
            Ok(_) => self.status = format!("opened {url}"),
            Err(e) => self.status = format!("could not run {opener}: {e}"),
        }
        Flow::Dirty
    }

    /// Ask before doing something that costs runner minutes or kills a build.
    ///
    /// A screen-local two-key confirm rather than a `tui-core` dialog, because
    /// there is no dialog in `tui-core` yet and this needs no text entry -- it
    /// needs one yes. It is safe by construction rather than by bookkeeping:
    /// the ONLY key that acts is `y`, and any other key the screen sees clears
    /// the prompt, so a stale prompt can never turn a later keystroke into a
    /// rerun. Moving the cursor clears it too, via `focus`.
    ///
    /// A shared confirm is still the right eventual home -- `pr-create`,
    /// `secret-set` and every delete need one WITH text entry, and that is a
    /// framework change rather than five screen-local copies of this.
    fn ask(&mut self, what: Confirm, run: &Run) -> Flow {
        let (verb, detail) = match what {
            Confirm::RerunFailed => ("rerun the failed jobs of", "they will run again"),
            Confirm::Rerun => ("rerun ALL jobs of", "the whole workflow runs again"),
            Confirm::Cancel => ("cancel", "jobs still running will be stopped"),
        };
        self.pending = Some(Pending {
            what,
            id: run.id,
            number: run.run_number,
        });
        self.status = format!(
            "{verb} #{} \u{2014} {detail}.  y to confirm, any other key to leave it",
            run.run_number
        );
        Flow::Dirty
    }

    /// The command a confirm stands for, as the fetcher is asked to run it.
    fn do_req(&self, asked: &Pending) -> Req {
        Req::Do {
            args: asked.argv(&self.repo),
        }
    }

    /// Do what the prompt said, to the run the prompt NAMED.
    ///
    /// Nothing here reads the list or the cursor. Between the prompt and the
    /// `y` a refresh can land and put a different run in the row.
    ///
    /// Sent, not waited for. `gh run rerun` is a network round trip, and run
    /// here it froze the screen for all of it -- no redraw, no keys, and a
    /// live run's clocks stopped. `hear_back` says how it went when it lands.
    fn act(&mut self, asked: Pending) -> Flow {
        self.fetch.send(self.do_req(&asked));
        self.status = format!(
            "asking github to {} #{}\u{2026}",
            match asked.what {
                Confirm::Cancel => "cancel",
                Confirm::Rerun | Confirm::RerunFailed => "rerun",
            },
            asked.number
        );
        self.sent.push(asked);
        Flow::Dirty
    }

    /// Say how each command that has come back went, and re-read the list
    /// for it. Run from `sync`, so on every arrival.
    fn hear_back(&mut self) {
        for asked in std::mem::take(&mut self.sent) {
            self.status = match self.fetch.get(&self.do_req(&asked)) {
                Some(Got::Pending) | None => {
                    self.sent.push(asked);
                    continue;
                }
                Some(Got::Failed(e)) => e,
                Some(Got::Denied(d)) => denied(&d),
                Some(_) => match asked.what {
                    Confirm::Cancel => format!("cancel requested for #{}", asked.number),
                    _ => format!("rerun triggered for #{}", asked.number),
                },
            };
            // A rerun makes a NEW attempt of the same id, so the list has to
            // be re-read before anything else is believed about this run. The
            // jobs cache needs no eviction -- it is keyed by attempt, so the
            // new attempt is simply a key nothing has answered yet.
            self.fetch.refresh(self.list_req());
        }
    }

    /// Every completed job's log, concatenated, for the pager.
    fn whole_log(&mut self, sel: usize) -> Option<String> {
        let run = self.run(sel)?.clone();
        let jobs = match self.jobs_of(&run) {
            Maybe::Got(jobs) => jobs,
            // Said out loud: a key that does nothing reads as a key that is
            // not bound.
            Maybe::Waiting => {
                self.status = "this run's jobs have not arrived yet".into();
                return None;
            }
            Maybe::Cannot(why) => {
                self.status = format!("could not list this run's jobs \u{2014} {why}");
                return None;
            }
        };
        let reqs: Vec<Req> = jobs
            .iter()
            .filter(|j| j.has_log())
            .map(|j| Req::Log { job: j.id })
            .collect();
        if reqs.is_empty() {
            self.status = "no completed job has a log yet".into();
            return None;
        }
        let all = self.fetch.block_on(&reqs, PAGER_WAIT);
        let mut out = String::new();
        for job in jobs.iter().filter(|j| j.has_log()) {
            out.push_str(&format!(
                "\n\u{2501}\u{2501}\u{2501} {} \u{2501}\u{2501}\u{2501}\n\n",
                logs::plain(&job.name)
            ));
            match self.log_of(job.id) {
                Maybe::Got(log) => {
                    for line in logs::fmt_log(log.lines()) {
                        out.push_str(&line);
                        out.push('\n');
                    }
                }
                Maybe::Waiting => out.push_str("(log did not arrive)\n"),
                Maybe::Cannot(why) => out.push_str(&format!(
                    "(could not fetch the log: {})\n",
                    logs::plain(&why)
                )),
            }
        }
        if !all {
            self.status = "some logs timed out".into();
        }
        Some(out)
    }
}

/// Write `body` somewhere a pager can read it, and return the argv to page it.
///
/// A temp FILE rather than a pipe, because `Flow::Detach` runs one command with
/// arguments and deliberately goes nowhere near a shell -- so there is nothing
/// to pipe into. `less -R` to keep the ANSI the log already carries.
pub fn pager(name: &str, body: &str) -> Option<Vec<String>> {
    let path = std::env::temp_dir().join(format!("github-{}-{name}.log", std::process::id()));
    std::fs::write(&path, body).ok()?;
    Some(vec![
        "less".into(),
        "-R".into(),
        path.to_string_lossy().into_owned(),
    ])
}

impl Screen for Actions {
    fn title(&self) -> String {
        format!("{} \u{b7} actions \u{b7} {}", self.repo, self.scope.label())
    }

    fn initial_sel(&self) -> usize {
        0
    }

    fn focus(&mut self, sel: usize) {
        if sel != self.sel {
            // Any cursor movement withdraws a pending confirm. The cheapest
            // honest rule: the prompt names a run, and the cursor is no longer
            // on it.
            self.pending = None;
            self.status.clear();
        }
        self.sel = sel;
        self.want_jobs(sel);
        // The jobs clock follows the cursor, because it is the SELECTED run
        // being watched -- moving to a finished run must stop the polling that
        // the live one started.
        self.jobs_due = self.watching(sel).then(|| Instant::now() + JOBS_EVERY);
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
        if self.list_due.is_some_and(|d| t >= d) {
            self.list_due = Some(t + LIST_EVERY);
            self.fetch.refresh(self.list_req());
            return Tick::Wake(IN_FLIGHT);
        }
        if self.jobs_due.is_some_and(|d| t >= d) {
            self.jobs_due = Some(t + JOBS_EVERY);
            if let Some(run) = self.run(self.sel) {
                self.fetch.refresh(self.jobs_req(run));
            }
            return Tick::Wake(IN_FLIGHT);
        }
        // Whichever clock is nearer, and nothing at all if neither is running.
        match [self.list_due, self.jobs_due]
            .into_iter()
            .flatten()
            .min()
            .map(|d| d.saturating_duration_since(t))
        {
            Some(d) => Tick::Wake(d),
            None => Tick::Idle,
        }
    }

    fn rows(&mut self) -> Vec<Row> {
        if let Some(e) = &self.error {
            return vec![Row::leaf("could not list runs").detail(logs::plain(e))];
        }
        if self.runs.is_empty() {
            let waiting = self.fetch.pending();
            return vec![Row::leaf(if waiting {
                "asking github\u{2026}"
            } else {
                "no workflow runs here yet"
            })];
        }
        // Showing the branch only when looking ACROSS branches: inside one
        // scope it is in the title bar and repeating it down the column says
        // nothing. Same rule as the root screen's slug.
        let across = matches!(self.scope, Scope::All);
        self.runs
            .iter()
            .map(|run| {
                let (badge, color) = run.verdict().badge();
                // The HEAD marker is a two-column prefix on every row, present
                // or blank, so the titles stay aligned. A marker that shifted
                // the column would be harder to scan than no marker at all.
                // Inside a commit scope every run IS that commit, so the
                // marker would sit on every row saying nothing. It earns its
                // column only where the list holds more than one commit.
                let here = !matches!(self.scope, Scope::Commit(_))
                    && self
                        .head_sha
                        .as_deref()
                        .is_some_and(|sha| sha == run.head_sha);
                // Inside one commit every run carries the SAME display title
                // -- the pull request's -- so the WORKFLOW is what identifies
                // a row there, and a column of five identical rows is what
                // this replaces. It goes in the LABEL rather than the detail
                // because the detail is the first thing a narrow list
                // truncates away, and the title is in the pane either way.
                //
                // With the run NUMBER, because a workflow can run several
                // times on one commit -- three `pull_request_target` runs of
                // the same workflow, measured on the first repo this was
                // pointed at -- and then the number is all there is. It is
                // spelt the way the pane's header spells it.
                let what = match (&self.scope, &run.name) {
                    (Scope::Commit(_), Some(name)) => {
                        format!("{name} #{}", run.run_number)
                    }
                    _ => run.display_title.clone(),
                };
                // A title is a commit message or a pull request's, and a
                // workflow's name is whatever its file says: text from
                // outside, so characters only. See `logs::plain`.
                let label = format!(
                    "{}{}",
                    if here { "\u{25cf} " } else { "  " },
                    logs::plain(&what)
                );
                let mut detail = time::age(run.age());
                if across {
                    if let Some(br) = &run.head_branch {
                        detail.push_str(&format!(" \u{b7} {}", logs::plain(br)));
                    }
                }
                Row::leaf(label).detail(detail).badged(badge, color)
            })
            .collect()
    }

    fn pane(&mut self, sel: usize, cols: u16, rows: u16) -> Pane {
        let mut lines: Vec<String> = Vec::new();
        if let Some(e) = &self.error {
            lines.push(format!("{}{e}{OFF}", crate::paint::RED));
            lines.push(String::new());
            lines.push(format!(
                "{DIM}`gh auth status` is the first thing to check.{OFF}"
            ));
        } else if let Some(run) = self.run(sel).cloned() {
            lines.extend(render::run_header(&run, cols, self.multi));
            let jobs = self.jobs_of(&run);
            // The header is already drawn, so the body is told how much room
            // is actually LEFT -- not how tall the pane is. A body that
            // budgeted against the whole pane would overflow by exactly the
            // header and lose its summary line.
            let room = rows.saturating_sub(lines.len() as u16);
            lines.extend(render::run_body(
                &run,
                jobs.view(|j| j.as_slice()),
                // The body takes a log by value, so it is copied here and
                // nowhere else.
                &|id| match self.log_of(id) {
                    Maybe::Got(log) => Maybe::Got(log.as_ref().clone()),
                    Maybe::Waiting => Maybe::Waiting,
                    Maybe::Cannot(why) => Maybe::Cannot(why),
                },
                cols,
                room,
            ));
        } else {
            lines.push(format!("{DIM}Nothing has run here yet.{OFF}"));
            lines.push(String::new());
            lines.push(format!(
                "{DIM}A workflow in .github/workflows and a push is all{OFF}"
            ));
            lines.push(format!("{DIM}it takes.{OFF}"));
        }
        // Under the error as well as under a run. The error pane used to
        // return before this, so what `r` and the keys said was never shown
        // on the one screen where something had gone wrong.
        if !self.status.is_empty() {
            lines.push(String::new());
            lines.push(format!("{ORG}{}{OFF}", self.status));
        }
        // The lines carry titles, names and gh's own words as well as this
        // crate's colours, so they leave through the same gate a log does.
        Pane {
            image: None,
            lines: lines.iter().map(|l| logs::clean(l)).collect(),
        }
    }

    fn on_action(&mut self, action: Action, sel: usize) -> Flow {
        // A pending confirm consumes the very next key the screen sees,
        // whatever it is. `y` acts; everything else withdraws.
        if let Some(asked) = self.pending.take() {
            self.status.clear();
            return match action {
                Action::Key('y') | Action::Key('Y') => self.act(asked),
                _ => {
                    self.status = "left alone".into();
                    Flow::Dirty
                }
            };
        }
        self.status.clear();
        match action {
            // `Activate` only; `l` moves into the pane. The pane IS this
            // screen's answer -- the run body with the failing step's log --
            // so being able to scroll it matters more than a second way to
            // drill in.
            Action::Activate => {
                let Some(run) = self.run(sel).cloned() else {
                    return Flow::Continue;
                };
                Flow::Push(Box::new(Steps::open(
                    &self.repo,
                    run,
                    self.list_req(),
                    self.multi,
                    Arc::clone(&self.fetch),
                )))
            }
            Action::Key('o') => self.browse(sel),
            Action::Key('r') => self.refresh(),
            Action::Key('b') => self.toggle_scope(),
            Action::Key('e') => {
                let Some(run) = self.run(sel).cloned() else {
                    return Flow::Continue;
                };
                // Rerunning only the failures is both cheaper and what you
                // nearly always mean, so it is what `e` offers when there ARE
                // failures -- and the prompt says which it is about to do.
                let failed = matches!(
                    self.jobs_of(&run),
                    Maybe::Got(jobs) if jobs
                        .iter()
                        .any(|j| j.verdict() == crate::model::Verdict::Failed)
                );
                let what = if failed {
                    Confirm::RerunFailed
                } else {
                    Confirm::Rerun
                };
                self.ask(what, &run)
            }
            Action::Key('x') => {
                let Some(run) = self.run(sel).cloned() else {
                    return Flow::Continue;
                };
                if run.verdict().done() {
                    self.status = "that run has already finished".into();
                    return Flow::Dirty;
                }
                self.ask(Confirm::Cancel, &run)
            }
            Action::Key('L') => {
                let name = self
                    .run(sel)
                    .map(|r| format!("run-{}", r.id))
                    .unwrap_or_default();
                match self.whole_log(sel).and_then(|body| pager(&name, &body)) {
                    Some(argv) => Flow::Detach(argv),
                    None => Flow::Dirty,
                }
            }
            _ => Flow::Continue,
        }
    }

    fn footer(&self) -> String {
        if self.pending.is_some() {
            return "y confirm \u{b7} any other key leave it".into();
        }
        // The footer names the RESULT, the way every toggle here does, so the
        // key does not have to be pressed to find out which way it goes.
        //
        // Read off where the key GOES, so the two cannot disagree, and left
        // out when it goes nowhere: a key the footer offers and the screen
        // refuses is worse than one not offered.
        let scope = match (&self.scope, self.next_scope()) {
            (_, None) => String::new(),
            (_, Some(Scope::All)) => " \u{b7} b all branches".into(),
            // Widening from a commit, as against narrowing from everything.
            (Scope::Commit(_), Some(Scope::Branch(b))) => format!(" \u{b7} b all of {b}"),
            (_, Some(Scope::Branch(b))) => format!(" \u{b7} b just {b}"),
            (_, Some(Scope::Commit(sha))) => format!(" \u{b7} b just {}", short(&sha)),
        };
        format!("enter jobs & steps \u{b7} o browser \u{b7} r refresh{scope}\ne rerun \u{b7} x cancel \u{b7} L whole log")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menu::check;
    use crate::model::{JobList, RunList};

    fn runs(states: &[(&str, Option<&str>)]) -> Vec<Run> {
        let items: Vec<String> = states
            .iter()
            .enumerate()
            .map(|(i, (status, concl))| {
                let c = concl
                    .map(|c| format!(r#","conclusion":"{c}""#))
                    .unwrap_or_default();
                format!(
                    r#"{{"id":{i},"display_title":"r{i}","head_sha":"sha{i}","event":"push",
                       "status":"{status}"{c},"html_url":"https://x",
                       "created_at":"2026-08-25T19:30:00Z","run_number":{i},"run_attempt":1}}"#
                )
            })
            .collect();
        serde_json::from_str::<RunList>(&format!(r#"{{"workflow_runs":[{}]}}"#, items.join(",")))
            .expect("parses")
            .workflow_runs
    }

    fn asked(what: Confirm, id: u64) -> Pending {
        Pending {
            what,
            id,
            number: id,
        }
    }

    fn jobs(states: &[&str]) -> Got {
        let items: Vec<String> = states
            .iter()
            .enumerate()
            .map(|(i, status)| {
                let c = if *status == "completed" {
                    r#","conclusion":"success""#
                } else {
                    ""
                };
                format!(r#"{{"id":{i},"name":"j{i}","status":"{status}"{c},"steps":[]}}"#)
            })
            .collect();
        Got::Jobs(Arc::new(
            serde_json::from_str::<JobList>(&format!(r#"{{"jobs":[{}]}}"#, items.join(",")))
                .expect("parses")
                .jobs,
        ))
    }

    /// Run 0's jobs as the fetcher keys them, whatever state the run is in.
    fn jobs_key() -> Req {
        Req::Jobs {
            run: 0,
            attempt: 1,
            persist: false,
        }
    }

    fn text(pane: Pane) -> String {
        pane.lines.join("\n")
    }

    /// Built without touching the network: these tests are about the clocks,
    /// and a screen that needed the API to be examined would not be testable.
    fn screen(states: &[(&str, Option<&str>)]) -> Actions {
        let runs = runs(states);
        let mut me = Actions {
            fetch: Arc::new(Fetcher::new("roest1/dotfiles")),
            repo: "roest1/dotfiles".into(),
            scope: Scope::All,
            branch: Some("main".into()),
            head_sha: Some("sha0".into()),
            commit: None,
            runs,
            list_seen: None,
            multi: false,
            error: None,
            status: String::new(),
            epoch_seen: 0,
            list_due: None,
            jobs_due: None,
            sel: 0,
            pending: None,
            sent: Vec::new(),
        };
        // What `sync` does with what arrived, minus the arriving.
        me.list_due = me
            .runs
            .iter()
            .any(|r| !r.verdict().done())
            .then(|| Instant::now() + LIST_EVERY);
        me.jobs_due = me
            .run(0)
            .map(|r| !r.verdict().done())
            .unwrap_or(false)
            .then(|| Instant::now() + JOBS_EVERY);
        me
    }

    /// The property that keeps this screen off the CPU, and the one a live
    /// refresh is most likely to break: a list of finished runs schedules
    /// NOTHING, so `tui-core`'s poll blocks for an hour and the process costs
    /// nothing until a key is pressed.
    #[test]
    fn a_finished_list_arms_no_clock_at_all() {
        let mut s = screen(&[
            ("completed", Some("success")),
            ("completed", Some("failure")),
        ]);
        assert!(s.list_due.is_none());
        assert!(s.jobs_due.is_none());
        assert!(matches!(s.tick(), Tick::Idle));
    }

    /// And the other direction: something in flight asks to be woken, on the
    /// slow clock rather than the frame clock.
    #[test]
    fn a_live_run_asks_to_be_woken_on_the_slow_clock() {
        let mut s = screen(&[("in_progress", None), ("completed", Some("success"))]);
        assert!(s.list_due.is_some());
        assert!(s.jobs_due.is_some());
        match s.tick() {
            // The nearer of the two clocks, which is the jobs one.
            Tick::Wake(d) => assert!(d <= JOBS_EVERY, "woke on {d:?}"),
            _ => panic!("a live run must schedule a wake"),
        }
    }

    /// The jobs clock follows the CURSOR, not the list. Moving onto a finished
    /// run has to stop the polling the live one started, or every run visited
    /// during a build leaves a timer behind it.
    #[test]
    fn moving_off_a_live_run_stops_polling_its_jobs() {
        let mut s = screen(&[("in_progress", None), ("completed", Some("success"))]);
        s.focus(1);
        assert!(s.jobs_due.is_none(), "still polling a finished run");
        // The LIST clock stays armed: run 0 is still going, and it is still on
        // the list whether or not the cursor is on it.
        assert!(s.list_due.is_some());
        s.focus(0);
        assert!(s.jobs_due.is_some(), "stopped polling a live run");
    }

    /// A confirm must not survive the keystroke after it. `y` is the only key
    /// that acts, so the dangerous direction is a prompt that stays up -- and
    /// the cheapest rule that closes it is that any key the screen sees, and
    /// any cursor movement, withdraws.
    #[test]
    fn a_confirm_is_withdrawn_by_anything_that_is_not_yes() {
        let mut s = screen(&[("in_progress", None)]);
        s.pending = Some(asked(Confirm::Cancel, 0));
        s.on_action(Action::Key('j'), 0);
        assert!(s.pending.is_none(), "a stray key left the prompt up");

        s.pending = Some(asked(Confirm::Cancel, 0));
        s.focus(1);
        assert!(s.pending.is_none(), "moving the cursor left the prompt up");
    }

    /// Cancelling something already finished is a no-op the screen should name
    /// rather than ask about.
    #[test]
    fn a_finished_run_is_not_offered_a_cancel() {
        let mut s = screen(&[("completed", Some("success"))]);
        s.on_action(Action::Key('x'), 0);
        assert!(s.pending.is_none());
        assert!(s.status.contains("already finished"), "{}", s.status);
    }

    /// Switching scope must drop the runs it was showing. Keeping them would
    /// leave the list showing one scope while the title bar claims another,
    /// which is the kind of wrong that gets believed.
    #[test]
    fn changing_scope_discards_the_runs_of_the_old_one() {
        let mut s = screen(&[
            ("completed", Some("success")),
            ("completed", Some("failure")),
        ]);
        assert_eq!(s.runs.len(), 2);
        s.sel = 1;
        let flow = s.on_action(Action::Key('b'), 1);
        assert!(matches!(s.scope, Scope::Branch(_)));
        assert!(s.runs.is_empty(), "showed the old scope's runs");
        assert_eq!(s.sel, 0);
        // And the cursor is sent where `sel` now says it is. Answering
        // `Dirty` left it on row 1 of a list this screen was reading from
        // row 0.
        assert!(matches!(flow, Flow::Retarget), "left the cursor behind");
        assert_eq!(s.initial_sel(), 0);
    }

    /// A screen the pull request menu opened must be able to get BACK. Its
    /// two scopes are the commit and the branch carrying it; widening to every
    /// branch would be a door that does not come back, because nothing on
    /// screen afterwards names the commit again.
    #[test]
    fn a_commit_scope_is_reversible() {
        let mut s = screen(&[("completed", Some("success"))]);
        s.scope = Scope::Commit("sha0".into());
        s.commit = Some("sha0".into());
        s.branch = Some("feat/x".into());

        s.on_action(Action::Key('b'), 0);
        assert_eq!(s.scope, Scope::Branch("feat/x".into()));
        assert!(s.footer().contains("b just sha0"), "{}", s.footer());

        s.on_action(Action::Key('b'), 0);
        assert_eq!(s.scope, Scope::Commit("sha0".into()));
        assert!(s.footer().contains("b all of feat/x"), "{}", s.footer());
    }

    /// With no branch to widen to -- a fork whose ref this clone has never
    /// seen -- `b` still has somewhere to go, and still comes back.
    #[test]
    fn a_commit_scope_with_no_branch_widens_to_everything() {
        let mut s = screen(&[("completed", Some("success"))]);
        s.scope = Scope::Commit("sha0".into());
        s.commit = Some("sha0".into());
        s.branch = None;

        s.on_action(Action::Key('b'), 0);
        assert_eq!(s.scope, Scope::All);
        assert!(s.status.is_empty(), "complained instead: {}", s.status);

        s.on_action(Action::Key('b'), 0);
        assert_eq!(s.scope, Scope::Commit("sha0".into()));
    }

    /// Inside a commit scope every run is that commit, so a marker saying so
    /// would be on every row and mean nothing. It comes back as soon as the
    /// list holds more than one commit.
    #[test]
    fn the_head_marker_is_off_inside_a_commit_scope() {
        let mut s = screen(&[
            ("completed", Some("success")),
            ("completed", Some("success")),
        ]);
        s.commit = Some("sha0".into());
        s.scope = Scope::Commit("sha0".into());
        assert!(
            s.rows().iter().all(|r| r.label.starts_with("  ")),
            "marked a row inside its own commit scope"
        );
        s.scope = Scope::Branch("main".into());
        assert!(s.rows()[0].label.starts_with('\u{25cf}'), "lost the marker");
    }

    /// Inside one commit every run carries the pull request's title, so a
    /// list of titles is a column of identical rows. The workflow is what
    /// identifies a row there -- in the LABEL, because the detail is the first
    /// thing a 52-column list truncates away.
    #[test]
    fn a_commit_scope_lists_workflows_rather_than_one_title_five_times() {
        let mut s = screen(&[
            ("completed", Some("success")),
            ("completed", Some("failure")),
        ]);
        for r in s.runs.iter_mut() {
            r.display_title = "Allow installation tokens to attach files".into();
        }
        s.runs[0].name = Some("CI".into());
        s.runs[1].name = Some("PR Triaging".into());
        s.scope = Scope::Commit("sha0".into());
        s.commit = Some("sha0".into());
        let rows = s.rows();
        assert_eq!(rows[0].label.trim(), "CI #0", "{rows:?}");
        assert_eq!(rows[1].label.trim(), "PR Triaging #1", "{rows:?}");

        // And the number alone carries it when the workflow does not: a
        // workflow can run several times on one commit.
        s.runs[1].name = Some("CI".into());
        let rows = s.rows();
        assert_eq!(rows[1].label.trim(), "CI #1", "{rows:?}");

        // Everywhere else the title is what differs between rows and the
        // workflow is the pane's business.
        s.scope = Scope::All;
        let rows = s.rows();
        assert!(rows[0].label.contains("Allow installation"), "{rows:?}");
    }

    /// Toggling BACK to a scope the fetcher has already answered has to
    /// re-read it. Nothing arrives, so nothing bumps the epoch `tick` syncs
    /// on, and the screen would sit on an empty list over an answer in hand.
    #[test]
    fn toggling_back_re_reads_an_answer_already_in_hand() {
        let mut s = screen(&[("completed", Some("success"))]);
        let branch = Scope::Branch("main".into());
        s.fetch.seed(
            Req::Runs {
                query: branch.query(),
            },
            Got::Runs(Arc::new(runs(&[
                ("completed", Some("success")),
                ("completed", Some("failure")),
            ]))),
        );
        s.on_action(Action::Key('b'), 0);
        assert_eq!(s.scope, branch);
        assert_eq!(
            s.runs.len(),
            2,
            "sat on an empty list over an answer it had"
        );
    }

    /// A detached HEAD has no branch to scope to, and saying so beats building
    /// a query for the branch `HEAD`.
    #[test]
    fn with_no_branch_the_scope_toggle_explains_itself() {
        let mut s = screen(&[("completed", Some("success"))]);
        s.branch = None;
        s.on_action(Action::Key('b'), 0);
        assert!(matches!(s.scope, Scope::All));
        assert!(s.status.contains("no branch"), "{}", s.status);
    }

    /// The marker that answers "is this MY run". It is on the row whose head
    /// sha is the checked-out commit and on no other.
    #[test]
    fn the_head_marker_lands_on_the_commit_you_are_standing_on() {
        let mut s = screen(&[
            ("completed", Some("success")),
            ("completed", Some("success")),
        ]);
        let rows = s.rows();
        assert!(rows[0].label.starts_with('\u{25cf}'), "{:?}", rows[0].label);
        // Blank rather than absent, so the titles stay in one column.
        assert!(rows[1].label.starts_with("  "), "{:?}", rows[1].label);
    }

    /// The rule, over every command this screen can run: it names its
    /// repository. `github` is launched from anywhere, and a `gh run` left to
    /// work out the repository from the working directory fails outside a
    /// clone and reaches the WRONG repository inside a different one.
    #[test]
    fn no_command_depends_on_the_working_directory() {
        for what in [Confirm::Rerun, Confirm::RerunFailed, Confirm::Cancel] {
            let argv = asked(what, 4242).argv("roest1/dotfiles");
            assert!(
                check::names_repo(&argv, "roest1/dotfiles"),
                "{what:?} names no repository: {argv:?}"
            );
            assert_eq!(
                &argv[..3],
                [
                    "run",
                    if what == Confirm::Cancel {
                        "cancel"
                    } else {
                        "rerun"
                    },
                    "4242"
                ]
            );
            assert_eq!(
                argv.iter().any(|a| a == "--failed"),
                what == Confirm::RerunFailed
            );
        }
    }

    /// `rows()` and `run()` are two readings of one list, in every shape the
    /// list takes. The error shape is the one that was wrong: one row saying
    /// the list could not be read, drawn over runs still held underneath.
    #[test]
    fn the_row_walk_agrees_with_the_rows_in_every_shape() {
        let mut s = screen(&[("in_progress", None), ("completed", Some("failure"))]);
        let rows = s.rows();
        assert_eq!(rows.len(), 2);
        for (i, row) in rows.iter().enumerate() {
            let run = s.run(i).expect("a run behind every row");
            assert!(row.label.contains(&run.display_title), "row {i}: {row:?}");
        }
        assert!(s.run(2).is_none());

        // The list could not be refreshed. One row, and nothing behind it.
        s.error = Some("gh: Bad credentials (HTTP 401)".into());
        let rows = s.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "could not list runs");
        assert!(
            s.run(0).is_none(),
            "a run is reachable behind the error row"
        );

        // Nothing has run here. One row, and nothing behind it.
        let mut empty = screen(&[]);
        assert_eq!(empty.rows().len(), 1);
        assert!(empty.run(0).is_none());
    }

    /// With the error row up, no key acts on a run: there is not one on
    /// screen. `e` then `y` used to rerun the first run of the list from
    /// before the failure, with the prompt itself never drawn.
    #[test]
    fn no_key_acts_on_a_run_hidden_behind_the_error_row() {
        let mut s = screen(&[("in_progress", None), ("completed", Some("failure"))]);
        s.error = Some("gh: Bad credentials (HTTP 401)".into());
        for key in ['e', 'x', 'o', 'L'] {
            s.on_action(Action::Key(key), 0);
            assert!(s.pending.is_none(), "{key} armed a confirm on a hidden run");
        }
        assert!(matches!(s.on_action(Action::Activate, 0), Flow::Continue));
        assert!(!s.watching(0), "still polling the jobs of a hidden run");

        // And whatever the screen has to say is said under the error too.
        s.status = "left alone".into();
        let pane = text(s.pane(0, 60, 40));
        assert!(pane.contains("Bad credentials"), "{pane}");
        assert!(
            pane.contains("left alone"),
            "the error pane hid the status: {pane}"
        );
    }

    /// The prompt names a run, and `y` acts on THAT run -- by id, wherever it
    /// is in the list by then. A refresh landing between the two keys used to
    /// hand the confirm to whatever had moved into the row.
    #[test]
    fn a_confirm_acts_on_the_run_it_named() {
        let mut s = screen(&[("in_progress", None), ("in_progress", None)]);
        s.on_action(Action::Key('x'), 1);
        assert!(s.status.contains("cancel #1"), "{}", s.status);
        // A push lands: a new run takes row 0 and everything moves down one.
        let mut newer = runs(&[("queued", None)]);
        newer[0].id = 99;
        newer[0].run_number = 99;
        newer.extend(s.runs.clone());
        s.runs = newer;
        assert_eq!(s.runs[1].id, 0, "row 1 is a different run now");

        let asked = s.pending.expect("still asking");
        assert_eq!(
            (asked.what, asked.id, asked.number),
            (Confirm::Cancel, 1, 1)
        );
        assert_eq!(
            asked.argv(&s.repo),
            ["run", "cancel", "1", "--repo", "roest1/dotfiles"]
        );
    }

    /// The list clock is re-armed by the LIST arriving, and by nothing else.
    /// Re-armed on every arrival, a live run's five-second jobs poll pushed
    /// the fifteen-second list deadline out each time it landed: the list was
    /// never re-asked, the run never showed as finished, and its jobs were
    /// polled for good.
    #[test]
    fn only_the_list_arriving_rearms_the_list_clock() {
        let mut s = screen(&[("in_progress", None)]);
        let list = |s: &Actions| Got::Runs(Arc::new(s.runs.clone()));
        s.fetch.seed(s.list_req(), list(&s));
        s.fetch.seed(jobs_key(), jobs(&["in_progress"]));
        s.sync();
        let armed = s.list_due.expect("a live run arms the list clock");

        // The jobs poll lands, and lands again. The list has not.
        for _ in 0..3 {
            std::thread::sleep(Duration::from_millis(2));
            s.fetch.seed(jobs_key(), jobs(&["in_progress"]));
            s.sync();
            assert_eq!(
                s.list_due,
                Some(armed),
                "a jobs arrival moved the list clock"
            );
            assert!(s.jobs_due.is_some(), "stopped watching jobs still going");
        }

        // Every job has completed, so asking for them again is asking for
        // the same thing: the jobs clock stops, though the list still calls
        // the run live.
        s.fetch.seed(jobs_key(), jobs(&["completed", "completed"]));
        s.sync();
        assert!(
            s.jobs_due.is_none(),
            "still polling jobs that have all finished"
        );
        assert_eq!(s.list_due, Some(armed));
        s.focus(0);
        assert!(s.jobs_due.is_none(), "the cursor restarted it");

        // The list arrives, and that does move its clock. It still says the
        // run is going -- later jobs of a `needs:` chain do not exist until
        // the earlier ones finish -- so the jobs are looked at once more.
        std::thread::sleep(Duration::from_millis(2));
        s.fetch.seed(s.list_req(), list(&s));
        s.sync();
        assert!(
            s.list_due.is_some_and(|d| d > armed),
            "the list did not re-arm"
        );
        assert!(s.jobs_due.is_some(), "gave up on a run the list calls live");
    }

    /// A list refused for want of a scope is an answer, and it is not "no
    /// workflow runs here yet". It used to be dropped on the floor.
    #[test]
    fn a_refused_list_is_said_to_be_refused() {
        let mut s = screen(&[]);
        s.list_due = Some(Instant::now());
        s.fetch.seed(
            s.list_req(),
            Got::Denied(Denied {
                accepted: vec!["repo".into()],
                suggest: "repo".into(),
            }),
        );
        s.sync();
        let rows = s.rows();
        assert_eq!(rows[0].label, "could not list runs");
        let why = rows[0].detail.clone().unwrap_or_default();
        assert!(
            why.contains("gh auth refresh -h github.com -s repo"),
            "{why}"
        );
        // Asking again on a timer will not grant a scope.
        assert!(s.list_due.is_none());
    }

    /// And the same one level in: jobs that could not be fetched are not
    /// jobs still being fetched.
    #[test]
    fn jobs_that_failed_to_arrive_are_not_shown_as_on_their_way() {
        let mut s = screen(&[("in_progress", None)]);
        s.fetch.seed(
            jobs_key(),
            Got::Failed("gh: Server Error (HTTP 502)".into()),
        );
        let pane = text(s.pane(0, 60, 40));
        assert!(!pane.contains("fetching jobs"), "{pane}");
        assert!(pane.contains("could not list this run's jobs"), "{pane}");
        assert!(pane.contains("HTTP 502"), "{pane}");
    }

    /// `r` says "refreshing", and the answer landing is what un-says it. The
    /// line used to stay up until the next key.
    #[test]
    fn the_refreshing_line_comes_down_when_the_list_lands() {
        let mut s = screen(&[]);
        s.status = REFRESHING.into();
        s.fetch.seed(s.list_req(), Got::Runs(Arc::new(Vec::new())));
        s.sync();
        assert!(s.status.is_empty(), "{}", s.status);
    }

    /// A request in flight keeps the screen awake on the short clock, with
    /// no slow clock running: an idle tick here is a refresh that is drawn
    /// on the next keypress.
    #[test]
    fn a_request_in_flight_is_not_an_idle_screen() {
        let mut s = screen(&[]);
        assert!(matches!(s.tick(), Tick::Idle));
        // `gh` refuses the flag without going anywhere.
        s.fetch.refresh(Req::Cmd {
            args: vec!["--definitely-not-a-gh-flag".into()],
        });
        match s.tick() {
            Tick::Wake(d) => assert_eq!(d, IN_FLIGHT),
            // It has already come back, which is the other honest answer.
            Tick::Changed => {}
            _ => panic!("went idle with a request in flight"),
        }
    }

    /// The footer names where `b` GOES, read off the same function the key
    /// uses. On a commit with no branch it said "just <sha>" for a key that
    /// widens to every branch; on a detached HEAD it offered "all branches"
    /// for a key that does nothing.
    #[test]
    fn the_footer_offers_b_only_where_b_goes() {
        let mut s = screen(&[("completed", Some("success"))]);
        s.scope = Scope::Commit("sha0".into());
        s.commit = Some("sha0".into());
        s.branch = None;
        assert_eq!(s.next_scope(), Some(Scope::All));
        assert!(s.footer().contains("b all branches"), "{}", s.footer());
        assert!(!s.footer().contains("b just"), "{}", s.footer());

        s.scope = Scope::All;
        s.commit = None;
        assert_eq!(s.next_scope(), None);
        assert!(!s.footer().contains(" b "), "{}", s.footer());
        assert!(s.footer().contains("r refresh"), "{}", s.footer());

        s.branch = Some("main".into());
        assert!(s.footer().contains("b just main"), "{}", s.footer());
        s.scope = Scope::Branch("main".into());
        assert!(s.footer().contains("b all branches"), "{}", s.footer());
    }

    /// A title is a commit message. Whatever is in one, the row holds
    /// characters and the pane holds characters and colours.
    #[test]
    fn a_title_cannot_carry_an_escape_to_the_terminal() {
        let mut s = screen(&[("completed", Some("success"))]);
        s.runs[0].display_title = "fix\u{1b}]52;c;QQ==\u{7}\u{1b}[2J it\tnow".into();
        s.runs[0].head_branch = Some("feat\u{1b}[Hx".into());
        s.fetch.seed(
            Req::Jobs {
                run: 0,
                attempt: 1,
                persist: true,
            },
            jobs(&["completed"]),
        );
        let rows = s.rows();
        assert!(rows[0].label.ends_with("fix it now"), "{:?}", rows[0].label);
        let detail = rows[0].detail.clone().unwrap_or_default();
        assert!(!detail.chars().any(char::is_control), "{detail:?}");
        for line in s.pane(0, 60, 40).lines {
            assert!(!line.contains("52;c") && !line.contains("[2J"), "{line:?}");
            assert!(!line.contains("[H"), "{line:?}");
        }
    }

    #[test]
    fn a_branch_scope_encodes_the_characters_a_branch_name_may_hold() {
        // `/` and `+` both change the meaning of a query parameter, and both
        // are legal in a branch name -- `feat/x+y` must not ask for `feat/x y`.
        assert_eq!(
            Scope::Branch("feat/github-tui".into()).query(),
            "per_page=40&branch=feat%2Fgithub-tui"
        );
        assert_eq!(
            Scope::Branch("fix+more".into()).query(),
            "per_page=40&branch=fix%2Bmore"
        );
        assert_eq!(Scope::All.query(), "per_page=40");
        assert_eq!(
            Scope::Commit("deadbeef".into()).query(),
            "per_page=40&head_sha=deadbeef"
        );
    }

    /// The label is what the title bar claims the list is, so it has to track
    /// the query exactly -- a short sha reads as a sha, not as a branch.
    #[test]
    fn the_label_names_the_scope_it_fetched() {
        assert_eq!(Scope::All.label(), "all branches");
        assert!(Scope::Branch("main".into()).label().contains("main"));
        assert_eq!(
            Scope::Commit("deadbeefcafe".into()).label(),
            "commit deadbee"
        );
        // A sha shorter than seven characters must not panic the slice.
        assert_eq!(Scope::Commit("abc".into()).label(), "commit abc");
    }

    /// `y` sends the command and comes straight back. It used to run gh
    /// there and then, on the thread that draws, and the screen stood still
    /// for the round trip. The command is the one the prompt named, and
    /// nothing is said to have happened until GitHub has said so.
    #[test]
    fn a_confirmed_rerun_is_sent_and_not_waited_for() {
        let mut s = screen(&[("completed", Some("failure")), ("in_progress", None)]);
        s.on_action(Action::Key('e'), 0);
        let asked = s.pending.expect("e asks first");
        let sent = Req::Do {
            args: asked.argv(&s.repo),
        };
        assert!(s.fetch.get(&sent).is_none(), "sent before the `y`");

        assert!(matches!(s.on_action(Action::Key('y'), 0), Flow::Dirty));
        assert!(s.fetch.flying(&sent), "y sent nothing");
        assert!(
            s.status.contains("asking github to rerun #0"),
            "{}",
            s.status
        );
        assert!(matches!(s.tick(), Tick::Wake(d) if d == IN_FLIGHT));
        // Something else lands first. That is not the answer.
        s.sync();
        assert!(s.status.contains("asking github"), "{}", s.status);
        assert!(!s.fetch.flying(&s.list_req()), "re-read before it was done");

        s.fetch
            .seed(sent, Got::Json(Arc::new(serde_json::Value::Null)));
        s.sync();
        assert_eq!(s.status, "rerun triggered for #0");
        // A rerun is a new attempt, so the list is read again.
        assert!(s.fetch.flying(&s.list_req()));
        assert!(s.sent.is_empty(), "reported once, not on every arrival");
    }

    /// A cancel that GitHub refused says so in gh's own words, and any key
    /// but `y` still sends nothing at all.
    #[test]
    fn a_refused_cancel_says_why_and_a_withdrawn_one_is_never_sent() {
        let mut s = screen(&[("in_progress", None)]);
        s.on_action(Action::Key('x'), 0);
        let sent = Req::Do {
            args: s.pending.expect("x asks first").argv(&s.repo),
        };
        s.on_action(Action::Key('n'), 0);
        assert!(s.fetch.get(&sent).is_none(), "a key that is not y sent it");
        assert!(s.sent.is_empty());

        s.on_action(Action::Key('x'), 0);
        s.on_action(Action::Key('y'), 0);
        assert!(s.fetch.flying(&sent));
        s.fetch.seed(
            sent,
            Got::Failed("gh: Cannot cancel a workflow run that is completed".into()),
        );
        s.sync();
        assert!(s.status.contains("Cannot cancel"), "{}", s.status);
        assert!(
            s.fetch.flying(&s.list_req()),
            "the list is read again either way"
        );
    }
}
