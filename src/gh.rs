//! Talking to GitHub, off the main thread, with the immutable half kept.
//!
//! ## Why it still shells out to `gh`
//!
//! Auth is solved over there, and solved for cases this would otherwise have to
//! learn: a token in the keyring, a token in the environment, an enterprise
//! host, a refresh mid-session. `gh api` returns the same JSON a direct call
//! would, and serde does not care who fetched it. Going direct would buy
//! nothing and cost an auth implementation -- which is the one kind of code
//! that fails for other people and not for you.
//!
//! ## Why it is threaded
//!
//! Every call here is a network round trip of 100ms to several seconds, and
//! `tui-core`'s loop must stay at 60fps through all of them. So requests go to
//! a small pool and results come back through a shared map, with an epoch
//! counter so the screen can tell "something landed" from "still waiting"
//! without diffing it. That is `font::fetch::Fetcher`'s shape, deliberately:
//! it is the same problem with a different payload, and a second design for it
//! would be a second thing to get wrong.
//!
//! ## What is cached across sessions, and why only that
//!
//! A COMPLETED run's jobs and a COMPLETED job's log can never change. They are
//! written to `~/.cache/dotfiles-github` and read from there forever, so
//! revisiting last week's failure is instant in a shell that has never seen
//! it. Nothing else is cached at all: a run list is a question about now, and
//! a live run's jobs are the thing you are watching change.
//!
//! The cache is keyed by `(run, attempt)`, not by run. A rerun is the SAME run
//! id with a new attempt and entirely new jobs, so a key on id alone would
//! serve the previous attempt's results for as long as the cache survived --
//! and the bash implementation had to delete cache files on rerun to avoid
//! exactly that. Putting the attempt in the key retires the deletion.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tui_core::Tick;

use crate::model::{Job, JobList, Run, RunList};
use crate::scopes::{self, Denied, Granted};

/// How often to look again while a `gh` call is in flight.
///
/// `Tick::Busy` is the obvious answer and is the wrong one here. It means "wake
/// me next frame", which is right for work that lands in milliseconds -- the
/// font browser rasterising a specimen -- and wasteful for a network round trip
/// measured in hundreds. Polling at 60fps through a one-second fetch is ~60
/// redraws to notice something that could not possibly have arrived sooner,
/// and it is the screen, not the loop, that knows which kind of work it started.
///
/// A tenth of a second is far below the point where a response landing feels
/// delayed, and it is a sixth of the frames.
pub const IN_FLIGHT: Duration = Duration::from_millis(100);

/// The most GitHub returns in one page, and what every list that can be long
/// asks for. A list that comes back this long may have been cut, and the
/// screen showing it says so.
pub const PAGE: usize = 100;

/// Percent-encode `s` for a URL: only the unreserved characters pass through,
/// and whichever of the rest the caller says mean themselves where this is
/// going.
pub(crate) fn percent(s: &str, keep: &[u8]) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ if keep.contains(&b) => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// How long a cached artifact stays useful. Two weeks is long enough to cover
/// "what broke when I was away" and short enough that the directory does not
/// become a disk-space question nobody asked for.
const KEEP_DAYS: u64 = 14;

pub fn cache_dir() -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cache"));
    base.join("dotfiles-github")
}

/// What one `gh api -i` call came back with.
struct Reply {
    ok: bool,
    headers: String,
    body: Vec<u8>,
    /// What `gh` said went wrong. See `first_line`.
    err: String,
}

impl Reply {
    fn granted(&self) -> Option<Granted> {
        scopes::header(&self.headers, "X-OAuth-Scopes").map(Granted::parse)
    }

    /// The scopes this endpoint said it would accept, any one of which is
    /// enough. Empty when it named none.
    fn accepted(&self) -> Vec<String> {
        scopes::header(&self.headers, "X-Accepted-OAuth-Scopes")
            .map(scopes::header_list)
            .unwrap_or_default()
    }

    /// Was this refused for want of a scope?
    ///
    /// Asked of the response itself, so there is no table of endpoint-to-scope
    /// to keep current -- and so a 404 that is really a permissions refusal
    /// stops being rendered as "you have none of these". See `scopes`.
    fn denied(&self) -> Option<Denied> {
        if self.ok {
            return None;
        }
        Denied::from(&self.accepted(), &self.granted().unwrap_or_default())
    }
}

/// Split `gh api -i` output into headers and body at the blank line.
///
/// CRLF first, since that is what HTTP sends and what `gh` relays; bare LF as a
/// fallback. Byte-wise rather than on a `String`, because a job log is not
/// necessarily UTF-8 and must survive the split intact.
fn split_reply(raw: &[u8]) -> (String, Vec<u8>) {
    for sep in [b"\r\n\r\n".as_slice(), b"\n\n".as_slice()] {
        if let Some(i) = raw.windows(sep.len()).position(|w| w == sep) {
            return (
                String::from_utf8_lossy(&raw[..i]).into_owned(),
                raw[i + sep.len()..].to_vec(),
            );
        }
    }
    (String::new(), raw.to_vec())
}

/// The first non-empty line of stderr, where `gh` puts the thing worth reading
/// -- `gh: Not Found (HTTP 404)` rather than a stack of request detail.
fn first_line(stderr: &[u8], otherwise: &str) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(otherwise)
        .trim()
        .to_string()
}

/// Why `gh` could not be started at all.
fn spawn_error(e: std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => "gh is not installed".to_string(),
        _ => format!("could not run gh: {e}"),
    }
}

/// One `gh api -i` call.
///
/// `-i` on every call, not just the ones expected to fail. The headers are what
/// carry the token's scopes and the endpoint's requirements, and a failure is
/// exactly when they matter -- asking for them only after a refusal would mean
/// a second round trip to explain the first one.
fn api(args: &[String]) -> Result<Reply, String> {
    let out = Command::new("gh")
        .arg("api")
        .arg("-i")
        .args(args)
        .output()
        .map_err(spawn_error)?;
    let (headers, body) = split_reply(&out.stdout);
    Ok(Reply {
        ok: out.status.success(),
        headers,
        body,
        err: first_line(&out.stderr, "gh api failed"),
    })
}

/// What a screen can ask for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Req {
    /// The run list. `query` is everything after the `?`, built by the screen
    /// so the scope it is showing and the scope it fetched cannot disagree.
    Runs { query: String },
    /// One run's jobs and their steps.
    ///
    /// `persist` is the caller's promise that this run has REACHED a terminal
    /// state, so the answer is immutable and may be written to disk. The screen
    /// knows it from the list it is already showing, which saves the second
    /// `gh api` call the bash implementation made to ask the run its own
    /// status. The staleness is safe in the one direction that matters: a list
    /// up to fifteen seconds old can say "running" about a run that has just
    /// finished, and then we merely decline to cache something we could have
    /// cached. It can never say "completed" about a run that is still going,
    /// because completed is terminal.
    Jobs {
        run: u64,
        attempt: u32,
        persist: bool,
    },
    /// One completed job's log, whole.
    Log { job: u64 },
    /// An arbitrary `gh` subcommand that prints JSON.
    ///
    /// For the things `gh` computes that the REST API does not hand over in
    /// one call. `gh pr list --json reviewDecision,statusCheckRollup` is the
    /// reason: both are GraphQL aggregates, and asking REST for them is a
    /// request per pull request. `gh` already does that work.
    ///
    /// Unlike `Json` this does NOT go through `gh api`, so there are no
    /// `X-OAuth-Scopes` headers on the way back and no `Got::Denied` from it.
    /// That is honest rather than a gap: a `gh pr list` that fails has failed
    /// for its own reasons, and gh says what they were on stderr.
    Cmd { args: Vec<String> },
    /// A `gh` subcommand run for what it DOES, not for what it prints.
    ///
    /// `gh run rerun` and `gh run cancel` are a network round trip each, and
    /// run on the main thread they froze the screen for it. They go through
    /// the pool like everything else, and the answer is only whether it
    /// worked: `Got::Json` of a null, or `Got::Failed` with gh's reason.
    ///
    /// Sent with `Fetcher::send`, never `request`: a command is not something
    /// to answer from the last time it was run.
    Do { args: Vec<String> },
    /// Any read-only endpoint whose result the caller parses itself.
    ///
    /// One variant rather than nine. The user and org screens are a dozen flat
    /// lists -- orgs, members, teams, keys, emails -- and a `Req` per endpoint
    /// would be a dozen near-identical arms whose only difference is a path and
    /// a type the SCREEN already knows. The Actions requests keep their own
    /// variants because they carry cache policy and an attempt number, which is
    /// real behaviour rather than a path.
    Json { path: String },
}

impl Req {
    fn key(&self) -> String {
        match self {
            Req::Runs { query } => format!("runs?{query}"),
            Req::Jobs { run, attempt, .. } => format!("jobs:{run}:{attempt}"),
            Req::Log { job } => format!("log:{job}"),
            Req::Json { path } => format!("json:{path}"),
            Req::Cmd { args } => format!("cmd:{}", args.join(" ")),
            Req::Do { args } => format!("do:{}", args.join(" ")),
        }
    }

    /// Is the caller promising this answer can no longer change?
    fn is_final(&self) -> bool {
        matches!(self, Req::Jobs { persist: true, .. })
    }

    /// Where this answer lives between sessions, if it is allowed to.
    fn cache_path(&self, repo: &str) -> Option<PathBuf> {
        // The user-level fetcher is not repo-scoped and caches nothing, so an
        // empty slug must not resolve to the cache root -- writing `job-7.log`
        // beside the per-repo directories would be a file nothing ever reads
        // and prune would then try to delete those directories as files.
        if repo.is_empty() {
            return None;
        }
        let dir = cache_dir().join(repo);
        match self {
            Req::Jobs {
                run,
                attempt,
                persist,
            } if *persist => Some(dir.join(format!("run-{run}-a{attempt}.jobs.json"))),
            // Only ever requested for a completed job, so it is already
            // immutable by the time we are here -- `Job::has_log` is the guard.
            Req::Log { job } => Some(dir.join(format!("job-{job}.log"))),
            // Not cached: every one of these is a question about now. An org's
            // membership and a token's scopes are exactly the things that must
            // not be answered from last week.
            _ => None,
        }
    }
}

#[derive(Clone)]
pub enum Got {
    Pending,
    Runs(Arc<Vec<Run>>),
    Jobs(Arc<Vec<Job>>),
    Log(Arc<String>),
    Json(Arc<serde_json::Value>),
    /// Refused for want of a scope, with the command that fixes it.
    ///
    /// Kept apart from `Failed` because the two want different words on screen
    /// and different actions from the reader: one is "your token cannot see
    /// this, here is the one line that changes that", the other is "something
    /// went wrong". Collapsing them is how `GET /user/keys` ends up rendered as
    /// "no SSH keys" on an account that has two.
    Denied(Denied),
    Failed(String),
}

/// Three answers, not two.
///
/// A fetch has three outcomes, and "not yet" and "cannot" want different
/// lines. The pane bodies used to be told about two of them, `Some` and
/// `None`. So a request that had FAILED was handed over as "not here yet", and
/// the pane said `fetching jobs…` for as long as anyone cared to look -- about
/// a run whose jobs were never coming, because the token could not read them
/// or the network was down. The Environments pane sat on `asking github…` the
/// same way, for a repo you are only allowed to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Maybe<T> {
    /// Asked for and not back yet.
    Waiting,
    Got(T),
    /// Not coming, and why: gh's own words, or the command that grants the
    /// scope the token was refused for.
    Cannot(String),
}

impl<T> Maybe<T> {
    /// A JSON answer read as `T`, or why there is not one.
    ///
    /// `parse` reads the document where it is: a `&Value` is already a
    /// `Deserializer`, so `T::deserialize(v)` needs no copy of it.
    pub fn of<F>(got: Option<Got>, parse: F) -> Self
    where
        F: FnOnce(&serde_json::Value) -> Result<T, serde_json::Error>,
    {
        match got {
            Some(Got::Json(v)) => match parse(&v) {
                Ok(t) => Maybe::Got(t),
                Err(e) => Maybe::Cannot(format!("unreadable: {e}")),
            },
            Some(Got::Denied(d)) => Maybe::Cannot(d.fix()),
            Some(Got::Failed(e)) => Maybe::Cannot(e),
            _ => Maybe::Waiting,
        }
    }

    /// The same answer, looking at what was got through `f`.
    ///
    /// The bodies take slices and `&str`; the screens hold `Arc`s and
    /// `String`s. This is `Option::as_deref` for a type with a third state.
    pub fn view<'a, U: ?Sized>(&'a self, f: impl FnOnce(&'a T) -> &'a U) -> Maybe<&'a U> {
        match self {
            Maybe::Waiting => Maybe::Waiting,
            Maybe::Got(t) => Maybe::Got(f(t)),
            Maybe::Cannot(why) => Maybe::Cannot(why.clone()),
        }
    }
}

/// What has been answered, and what is on its way.
#[derive(Default)]
struct State {
    answers: HashMap<String, Got>,
    /// Every request a worker has not answered yet, first ask or refresh.
    ///
    /// Kept apart from `Got::Pending` because a refresh deliberately leaves the
    /// old answer in the map to go on being drawn -- so nothing in the map
    /// says one is in flight, and for a while nothing anywhere did. The screen
    /// then went idle, the app blocked for an hour, and the new list was drawn
    /// on the next keypress: `r` sat on "refreshing…" until you touched a key.
    flying: HashSet<String>,
    /// Jobs answers fetched once their run had FINISHED, so that is what they
    /// are for good. One fetched while the run was live is a snapshot.
    settled: HashSet<String>,
}

type Shared = Arc<Mutex<State>>;

pub struct Fetcher {
    repo: String,
    tx: Sender<Req>,
    state: Shared,
    /// Bumped by a worker whenever anything lands, so a screen can notice in
    /// one atomic read instead of walking the map every frame.
    ///
    /// Bumped while `state` is LOCKED, in the same breath as the request
    /// leaving `flying`, so "nothing in flight" always implies "whatever
    /// landed has already moved this".
    epoch: Arc<AtomicU64>,
    /// The epoch as of the last time a screen read it. See `pending`.
    read: AtomicU64,
    /// The scopes the last response said this token has. Learned from the
    /// headers of whatever call happened to run, never probed for.
    granted: Arc<Mutex<Granted>>,
}

fn parse(req: &Req, bytes: Vec<u8>) -> Got {
    match req {
        Req::Runs { .. } => match serde_json::from_slice::<RunList>(&bytes) {
            Ok(l) => Got::Runs(Arc::new(l.workflow_runs)),
            Err(e) => Got::Failed(format!("unreadable run list: {e}")),
        },
        Req::Jobs { .. } => match serde_json::from_slice::<JobList>(&bytes) {
            Ok(l) => Got::Jobs(Arc::new(l.jobs)),
            Err(e) => Got::Failed(format!("unreadable job list: {e}")),
        },
        // A log is bytes, not JSON, and is not necessarily valid UTF-8 -- a
        // test printing raw output can put anything in it. Lossy rather than
        // an error: the surrounding lines are still what you came to read.
        Req::Log { .. } => Got::Log(Arc::new(String::from_utf8_lossy(&bytes).into_owned())),
        Req::Json { path } => match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(v) => Got::Json(Arc::new(v)),
            Err(e) => Got::Failed(format!("unreadable response from {path}: {e}")),
        },
        Req::Cmd { args } => match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(v) => Got::Json(Arc::new(v)),
            Err(e) => Got::Failed(format!("unreadable output from gh {}: {e}", args.join(" "))),
        },
        // It worked, which is the whole answer. What `gh run rerun` prints
        // on the way is prose for a terminal and is not read.
        Req::Do { .. } => Got::Json(Arc::new(serde_json::Value::Null)),
    }
}

/// One `gh <subcommand>` call, for the requests `gh api` never sees.
fn cmd(req: &Req, args: &[String]) -> Got {
    // What to say when gh failed and said nothing.
    let silent = match req {
        Req::Do { .. } => "gh refused",
        _ => "gh failed",
    };
    match Command::new("gh").args(args).output() {
        Ok(o) if o.status.success() => parse(req, o.stdout),
        Ok(o) => Got::Failed(first_line(&o.stderr, silent)),
        Err(e) => Got::Failed(spawn_error(e)),
    }
}

fn fetch(repo: &str, req: &Req, seen: &Mutex<Granted>) -> Got {
    let args: Vec<String> = match req {
        // `gh <subcommand>`, not `gh api`, so they take the other route.
        Req::Cmd { args } | Req::Do { args } => return cmd(req, args),
        Req::Runs { query } => vec![format!("repos/{repo}/actions/runs?{query}")],
        Req::Jobs { run, .. } => vec![format!("repos/{repo}/actions/runs/{run}/jobs?per_page=100")],
        Req::Log { job } => vec![
            "--allow-escape-sequences".into(),
            format!("repos/{repo}/actions/jobs/{job}/logs"),
        ],
        Req::Json { path } => vec![path.clone()],
    };
    let reply = match api(&args) {
        Ok(r) => r,
        Err(e) => return Got::Failed(e),
    };
    // The token's scopes ride along on EVERY response, so they cost no call of
    // their own -- there is no startup probe here and does not need to be one.
    if let Some(g) = reply.granted() {
        if let Ok(mut slot) = seen.lock() {
            *slot = g;
        }
    }
    if let Some(d) = reply.denied() {
        return Got::Denied(d);
    }
    if !reply.ok {
        return Got::Failed(reply.err);
    }
    // Written only after it parses. A truncated or error-page body cached as if
    // it were an answer is a failure that survives restarts, which is the worst
    // kind this file can produce.
    let got = parse(req, reply.body.clone());
    if !matches!(got, Got::Failed(_)) {
        if let Some(path) = req.cache_path(repo) {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let tmp = path.with_extension("part");
            if std::fs::write(&tmp, &reply.body).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }
    got
}

impl Fetcher {
    /// Four workers. A run's body wants its jobs and the logs of two or three
    /// failed jobs at once, so a pool of one would serialise exactly the case
    /// the screen is built around; much more than four and a repo with twenty
    /// failed jobs would open twenty connections to be polite about.
    pub fn new(repo: &str) -> Self {
        let (tx, rx) = mpsc::channel::<Req>();
        let state: Shared = Arc::default();
        let epoch = Arc::new(AtomicU64::new(0));
        let granted: Arc<Mutex<Granted>> = Arc::new(Mutex::new(Granted::default()));
        let rx = Arc::new(Mutex::new(rx));
        // No workers under test, so a test cannot run `gh`. Screens ask for
        // things as they are built, and with a pool behind them one `cargo
        // test` was a hundred-odd real calls on whoever's token was to hand.
        // A request simply stays in flight; tests put answers in with `seed`.
        let workers = if cfg!(test) { 0 } else { 4 };
        for _ in 0..workers {
            let rx = Arc::clone(&rx);
            let state = Arc::clone(&state);
            let epoch = Arc::clone(&epoch);
            let granted = Arc::clone(&granted);
            let repo = repo.to_string();
            thread::spawn(move || loop {
                // The lock is held only for the recv, never across the fetch,
                // or the pool would be a queue of one.
                let req = {
                    let guard = match rx.lock() {
                        Ok(g) => g,
                        Err(_) => return,
                    };
                    match guard.recv() {
                        Ok(r) => r,
                        Err(_) => return,
                    }
                };
                let got = fetch(&repo, &req, &granted);
                let Ok(mut g) = state.lock() else { return };
                let key = req.key();
                if req.is_final() && !matches!(got, Got::Failed(_) | Got::Denied(_)) {
                    g.settled.insert(key.clone());
                }
                g.flying.remove(&key);
                g.answers.insert(key, got);
                epoch.fetch_add(1, Ordering::Relaxed);
            });
        }
        // Nor does a test touch the real cache directory.
        if !cfg!(test) {
            prune(repo);
        }
        Self {
            repo: repo.to_string(),
            tx,
            state,
            epoch,
            read: AtomicU64::new(0),
            granted,
        }
    }

    /// What this token may do, as of the last response that carried the header.
    ///
    /// Empty until the first call lands, so a screen asking about scopes has to
    /// treat "not known yet" as distinct from "none" -- rendering the second
    /// for the first would tell you your token has no permissions while the
    /// request proving otherwise is still in flight.
    pub fn granted(&self) -> Granted {
        self.granted.lock().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn epoch(&self) -> u64 {
        let now = self.epoch.load(Ordering::Relaxed);
        self.read.store(now, Ordering::Relaxed);
        now
    }

    /// Has anything landed since `seen`? The first half of every `tick`.
    ///
    /// It reads the epoch, which is what lets `waiting` and `pending` mean
    /// "still in flight" for the rest of that tick -- so it goes FIRST, and a
    /// screen that calls this and then `waiting` cannot get the order wrong.
    pub fn landed(&self, seen: &mut u64) -> bool {
        let now = self.epoch();
        if now == *seen {
            return false;
        }
        *seen = now;
        true
    }

    /// What a screen with nothing new to draw and no clock of its own answers
    /// from `tick`: the short clock while anything is on its way, or an answer
    /// landed that nobody has read yet, and idle otherwise.
    ///
    /// Without the first half a tab goes idle on "fetching…" with the answer
    /// still on its way, and draws it on the next keypress.
    pub fn waiting(&self) -> Tick {
        if self.pending() {
            Tick::Wake(IN_FLIGHT)
        } else {
            Tick::Idle
        }
    }

    pub fn get(&self, req: &Req) -> Option<Got> {
        self.state.lock().ok()?.answers.get(&req.key()).cloned()
    }

    /// Put an answer in by hand. Tests only.
    ///
    /// The only other way in is `request`, which reaches the network, so how a
    /// screen behaves on an answer it ALREADY HAS is otherwise untestable --
    /// and "already has" is exactly what toggling back to a scope looks like.
    /// It deliberately does NOT bump the epoch, because nothing arrived: that
    /// is the condition the screens have to cope with rather than a quirk of
    /// the seam.
    #[cfg(test)]
    pub fn seed(&self, req: Req, got: Got) {
        if let Ok(mut guard) = self.state.lock() {
            guard.answers.insert(req.key(), got);
        }
    }

    /// Call everything in flight landed, without an answer. Tests only: there
    /// are no workers under test, so nothing asked for ever comes back.
    #[cfg(test)]
    pub(crate) fn settle(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.flying.clear();
        }
    }

    /// Is this one request on its way? Tests only: `pending` answers for all
    /// of them at once, and a refresh leaves its old answer in the map.
    #[cfg(test)]
    pub(crate) fn flying(&self, req: &Req) -> bool {
        self.state
            .lock()
            .is_ok_and(|s| s.flying.contains(&req.key()))
    }

    /// Is there anything a screen should stay awake for?
    ///
    /// Something in flight, or something that LANDED since a screen last read
    /// the epoch. The second half closes a gap every screen's `tick` has: it
    /// reads `epoch()` and then asks this, and an answer arriving between the
    /// two was seen by neither -- the epoch had already been read, and by now
    /// nothing was in flight. The screen went idle on an answer it never drew.
    /// It was a window of microseconds, which is to say a hang nobody could
    /// reproduce.
    pub fn pending(&self) -> bool {
        let Ok(state) = self.state.lock() else {
            return false;
        };
        !state.flying.is_empty()
            || self.epoch.load(Ordering::Relaxed) != self.read.load(Ordering::Relaxed)
    }

    /// Ask once. Idempotent, so a screen can request the same thing on every
    /// cursor move -- which is exactly what prefetching around the selection
    /// does -- without re-fetching anything.
    pub fn request(&self, req: Req) {
        let Ok(mut guard) = self.state.lock() else {
            return;
        };
        let key = req.key();
        if guard.answers.contains_key(&key) {
            // A run watched to its finish has only its last LIVE snapshot,
            // which can still show a job running. Asked for as final, and not
            // yet fetched as final, it is fetched once more.
            if req.is_final() && !guard.settled.contains(&key) && guard.flying.insert(key) {
                drop(guard);
                let _ = self.tx.send(req);
            }
            return;
        }
        // Resolved from disk HERE rather than in the worker, which is the
        // lesson `Fetcher::request` in `font` already paid for: leaving it to
        // the thread meant something cached in a previous session still went
        // Pending -> queue -> thread -> Ready, so every revisit flashed
        // "fetching…" for no reason. A file read costs microseconds.
        if let Some(got) = self.cached(&req) {
            // Only what cannot change is ever on disk.
            guard.settled.insert(key.clone());
            guard.answers.insert(key, got);
            return;
        }
        guard.flying.insert(key.clone());
        guard.answers.insert(key, Got::Pending);
        drop(guard);
        let _ = self.tx.send(req);
    }

    /// Ask again, ignoring what we already have.
    ///
    /// For the two clocks the Actions screen runs on: the run list going stale,
    /// and a live run's jobs changing under the cursor. It deliberately does
    /// NOT clear the existing answer -- the screen keeps rendering the last
    /// good one while the new one is in flight, so a refresh is invisible
    /// rather than a flicker through an empty pane.
    pub fn refresh(&self, req: Req) {
        let Ok(mut guard) = self.state.lock() else {
            return;
        };
        let key = req.key();
        if !guard.flying.insert(key.clone()) {
            return; // already in flight; a second copy would just race it
        }
        guard.answers.entry(key).or_insert(Got::Pending);
        drop(guard);
        let _ = self.tx.send(req);
    }

    /// Send a command, and take down whatever the last one like it answered.
    ///
    /// `refresh` keeps the old answer on show while the new one is in flight,
    /// which is right for a list and wrong for a command: its answer is
    /// whether THIS one worked, and a second rerun of a run would otherwise
    /// be told the first one's result before it had left the machine.
    ///
    /// The same command already on its way is not sent twice.
    pub fn send(&self, req: Req) {
        let Ok(mut guard) = self.state.lock() else {
            return;
        };
        let key = req.key();
        if !guard.flying.insert(key.clone()) {
            return;
        }
        guard.answers.insert(key, Got::Pending);
        drop(guard);
        let _ = self.tx.send(req);
    }

    /// Ask for all of these and WAIT, up to `limit`.
    ///
    /// The one blocking call here, and it exists for one caller: paging a log.
    /// Everything else resolves into the pane asynchronously because the pane
    /// is redrawn constantly anyway, but a pager has to hand the terminal over
    /// with the whole text in hand -- there is no later frame to fill in.
    ///
    /// Blocking on an explicit keypress is honest in a way that blocking on
    /// cursor movement is not: you pressed a key and asked for a thing. The
    /// requests still go through the pool, so several logs arrive in parallel
    /// rather than one after another, and the timeout means a hung network
    /// costs a few seconds rather than the session.
    pub fn block_on(&self, reqs: &[Req], limit: std::time::Duration) -> bool {
        for r in reqs {
            self.request(r.clone());
        }
        let deadline = std::time::Instant::now() + limit;
        loop {
            let settled = reqs
                .iter()
                .all(|r| !matches!(self.get(r), Some(Got::Pending) | None));
            if settled {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            thread::sleep(std::time::Duration::from_millis(25));
        }
    }

    fn cached(&self, req: &Req) -> Option<Got> {
        if cfg!(test) {
            return None;
        }
        let path = req.cache_path(&self.repo)?;
        let bytes = std::fs::read(&path).ok()?;
        if bytes.is_empty() {
            return None;
        }
        let got = parse(req, bytes);
        // A cache file that no longer parses -- a format change, a truncated
        // write from a killed process -- is treated as absent rather than as
        // an error, so the next request repairs it.
        (!matches!(got, Got::Failed(_))).then_some(got)
    }
}

/// Drop cached artifacts older than `KEEP_DAYS`. Run by `Fetcher::new`, so
/// once for each repo opened and only over that repo's directory.
///
/// Best-effort throughout: a cache that cannot be pruned is a disk-space
/// question, and failing to open a screen over it would be the wrong trade.
fn prune(repo: &str) {
    if repo.is_empty() {
        return;
    }
    let dir = cache_dir().join(repo);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let cutoff = std::time::Duration::from_secs(KEEP_DAYS * 86_400);
    for e in entries.flatten() {
        let stale = e
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().map(|age| age > cutoff).unwrap_or(false))
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key is the cache's correctness. Two attempts of one run are two
    /// different sets of jobs, and `persist` must not be part of the key --
    /// a run fetched while live and again once finished is the same question,
    /// and keying on it would fetch twice and cache neither.
    #[test]
    fn a_rerun_is_a_different_key_but_persistence_is_not() {
        let live = Req::Jobs {
            run: 7,
            attempt: 1,
            persist: false,
        };
        let done = Req::Jobs {
            run: 7,
            attempt: 1,
            persist: true,
        };
        let again = Req::Jobs {
            run: 7,
            attempt: 2,
            persist: true,
        };
        assert_eq!(live.key(), done.key());
        assert_ne!(done.key(), again.key());
    }

    /// A refresh keeps the old answer on show, so the map alone cannot say one
    /// is in flight. `pending` has to, or the screen that asked goes idle and
    /// the answer is drawn on the next keypress.
    #[test]
    fn a_refresh_in_flight_is_pending_though_the_old_answer_stays() {
        let f = Fetcher::new("");
        let req = Req::Cmd {
            args: vec!["--definitely-not-a-gh-flag".into()],
        };
        f.seed(req.clone(), Got::Failed("the old answer".into()));
        assert!(!f.pending(), "nothing has been asked yet");
        f.refresh(req.clone());
        assert!(f.pending(), "a refresh in flight went unreported");
        assert!(
            f.get(&req).is_some_and(|got| !matches!(got, Got::Pending)),
            "the answer on show was cleared while the new one was in flight"
        );
    }

    /// An answer that lands between a screen reading the epoch and asking
    /// whether anything is pending must still keep that screen awake.
    #[test]
    fn an_answer_nobody_has_read_yet_is_still_pending() {
        let f = Fetcher::new("");
        let seen = f.epoch();
        // What a worker does when an answer lands.
        f.epoch.fetch_add(1, Ordering::Relaxed);
        assert!(f.pending(), "a landed answer was not reported");
        assert_ne!(f.epoch(), seen);
        assert!(!f.pending(), "read, and nothing in flight");
    }

    /// The two halves of a tick, in the order every screen uses them: an
    /// arrival is reported once, and a request in flight is the short clock
    /// rather than an idle screen.
    #[test]
    fn a_tick_sees_an_arrival_once_and_stays_awake_for_what_is_in_flight() {
        let f = Fetcher::new("");
        let mut seen = 0;
        assert!(!f.landed(&mut seen), "nothing has landed");
        assert!(matches!(f.waiting(), Tick::Idle));
        f.refresh(Req::Json {
            path: "user".into(),
        });
        assert!(!f.landed(&mut seen), "asking is not an arrival");
        assert!(matches!(f.waiting(), Tick::Wake(d) if d == IN_FLIGHT));
        // What a worker does when an answer lands.
        f.settle();
        f.epoch.fetch_add(1, Ordering::Relaxed);
        // Not yet read, so still worth waking for.
        assert!(matches!(f.waiting(), Tick::Wake(_)));
        assert!(f.landed(&mut seen), "an arrival went unreported");
        assert_eq!(seen, 1);
        assert!(!f.landed(&mut seen), "one arrival was reported twice");
        assert!(matches!(f.waiting(), Tick::Idle));
    }

    /// A command's answer is whether THAT command worked. Sending it again
    /// takes the old answer down, where a refresh would have left it up to
    /// be read as the new one's; and nothing about it is kept or parsed.
    #[test]
    fn a_command_sent_again_does_not_answer_with_the_last_ones_result() {
        let f = Fetcher::new("roest1/dotfiles");
        let rerun = Req::Do {
            args: vec!["run".into(), "rerun".into(), "7".into()],
        };
        assert_eq!(rerun.key(), "do:run rerun 7");
        assert!(rerun.cache_path("roest1/dotfiles").is_none());
        assert!(!rerun.is_final());
        // Whatever gh printed, success is the answer.
        assert!(
            matches!(parse(&rerun, b"\xe2\x9c\x93 Requested rerun".to_vec()), Got::Json(v) if v.is_null())
        );

        f.seed(rerun.clone(), Got::Json(Arc::new(serde_json::Value::Null)));
        f.send(rerun.clone());
        assert!(f.flying(&rerun), "it was not sent");
        assert!(
            matches!(f.get(&rerun), Some(Got::Pending)),
            "the first rerun's answer is standing in for the second's"
        );
    }

    /// Only the immutable answers get a file. A run list is a question about
    /// now, and a live run's jobs are the thing being watched change -- caching
    /// either would show yesterday's CI with total confidence.
    #[test]
    fn only_what_cannot_change_is_written_to_disk() {
        let repo = "roest1/dotfiles";
        assert!(Req::Runs {
            query: "per_page=40".into()
        }
        .cache_path(repo)
        .is_none());
        assert!(Req::Jobs {
            run: 7,
            attempt: 1,
            persist: false
        }
        .cache_path(repo)
        .is_none());
        let jobs = Req::Jobs {
            run: 7,
            attempt: 2,
            persist: true,
        }
        .cache_path(repo)
        .expect("completed jobs are cached");
        assert!(
            jobs.ends_with("roest1/dotfiles/run-7-a2.jobs.json"),
            "{jobs:?}"
        );
        let log = Req::Log { job: 99 }
            .cache_path(repo)
            .expect("logs are cached");
        assert!(log.ends_with("roest1/dotfiles/job-99.log"), "{log:?}");
    }
}
