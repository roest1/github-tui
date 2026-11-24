//! `github` — GitHub for the machine you are on.
//!
//! The Rust port of `bash/bash_github_tui`, built root-first and then by the
//! screen that stresses the most machinery. What exists today:
//!
//!   * the USER level — your repos, orgs, account and keys. Always the bottom
//!     of the stack, so standing in a repo seeds the SELECTION rather than
//!     making a second root that could not connect to the first;
//!   * the ROOT screen — every clone under `~/github`, grouped by where you
//!     filed it, read from disk with no API call in front of the first frame;
//!   * one repo as five tabs — BRANCHES (a full-width table, local git
//!     first), PULL REQUESTS (filters in a sidebar), ACTIONS (workflow runs
//!     with the failing step's log already in the pane, refreshing itself
//!     while a run is live), SECRETS (the repo's and each environment's) and
//!     ENVIRONMENTS (the deployment gates, with a picker for the reviewers);
//!   * the JOBS & STEPS screen — one run as a tree, with a pager on any log.
//!
//! The bash implementation is gone: the swap happened on 2026-10-24, and this
//! is the only `github` there is. `docs/decisions/github-parity.md` was the
//! gate for that and is now the record of it -- a row per verb the bash
//! version had, and for the four that were dropped, the argument for dropping
//! them.
//!
//! See `docs/decisions/github-tui.md` for the port's shape and what is
//! deliberately not being carried over.

pub mod branches;
pub mod envs;
pub mod gh;
pub mod index;
pub mod logs;
pub mod menu;
pub mod model;
pub mod orgs;
pub mod paint;
pub mod prs;
pub mod render;
pub mod root;
pub mod rules;
pub mod runs;
pub mod scopes;
pub mod secrets;
pub mod steps;
pub mod time;
pub mod user;

/// Hand a URL to the desktop's browser, and say what happened.
///
/// `xdg-open` on Linux, `open` on macOS -- the same pair `_open_url` in
/// `bash_github` picked between. Not waited for on this thread: that would
/// block the loop for as long as the browser takes to start, which on a cold
/// Firefox is seconds.
///
/// One copy, because there were four and the fifth would have been in a screen
/// that had no other reason to know what an operating system is. The return
/// value is a status line rather than a `Result` because every caller does the
/// same thing with it -- shows it. "opened" means the opener was started, which
/// is all that can be known without waiting for it.
pub fn browse(url: &str) -> String {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    match spawn_apart(std::process::Command::new(opener).arg(url)) {
        Ok(_) => format!("opened {url}"),
        Err(e) => format!("could not run {opener}: {e}"),
    }
}

/// Start a command that shares nothing with this terminal, and reap it.
///
/// All three streams go to `/dev/null`. Inherited, whatever the opener or the
/// browser it starts decides to print -- a GTK warning, a sandbox notice --
/// lands in the middle of the alternate screen, over rows the loop has no
/// reason to redraw, and stays there.
///
/// And it is waited for, on a thread of its own, because a child nobody waits
/// for stays in the process table as a zombie until this process exits: one
/// per `o`, for the length of the session. The thread lives exactly as long
/// as the child. The handle is returned so a test can wait for that; `browse`
/// drops it.
fn spawn_apart(cmd: &mut std::process::Command) -> std::io::Result<std::thread::JoinHandle<()>> {
    use std::process::Stdio;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(std::thread::spawn(move || {
        let _ = child.wait();
    }))
}

/// The tab mark: `gh_tui`, an OSC 1337 user var that wezterm reads.
///
/// `wezterm/wezterm.lua`'s `format-tab-title` puts GitHub's Octicons mark on
/// the tab while a github session is running, and it learns that a session is
/// running from this variable and nothing else. The bash implementation set it
/// in `_gh_tab_var`, which is NOT one of `_ghw_main`'s verbs -- so the parity
/// table, derived from that dispatcher, could not see it. It was found by
/// grepping for every mention of the file about to be deleted.
///
/// Two things make this worth a type rather than two calls. The variable is
/// process-scoped state inside somebody else's program: a mark left set
/// leaves the pane wearing the GitHub glyph for the life of the shell, which
/// is why the bash version hung the clear on a `RETURN` trap rather than
/// putting it at the bottom of the function. `Drop` is that trap -- it runs on
/// the way out of `main` and on an unwinding panic alike.
///
/// It writes to `/dev/tty` rather than stdout, for the same reason
/// `_gh_tab_var` did: the escape is addressed to the terminal, so it must not
/// land in a pipe when the output is redirected.
pub struct TabMark;

/// base64 of `1` and `0`. The var is a string and the Lua compares it to
/// `"1"`, so the value is encoded exactly as `_gh_tab_var` encoded it.
const MARK_ON: &str = "MQ==";
const MARK_OFF: &str = "MA==";

impl TabMark {
    pub fn set() -> Self {
        write_tty(&var_escape(MARK_ON));
        TabMark
    }
}

impl Drop for TabMark {
    fn drop(&mut self) {
        write_tty(&var_escape(MARK_OFF));
    }
}

/// The wire format wezterm parses, split out so a test can pin it.
fn var_escape(value: &str) -> String {
    format!("\u{1b}]1337;SetUserVar=gh_tui={value}\u{7}")
}

/// Best-effort: no tty, or one that cannot be opened for writing, means no
/// mark. There is nothing to report and nobody to report it to.
fn write_tty(s: &str) {
    use std::io::Write;
    if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        let _ = tty.write_all(s.as_bytes());
        let _ = tty.flush();
    }
}

pub use root::Root;
pub use runs::Actions;

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire format, pinned. `wezterm/wezterm.lua` reads it, so a change
    /// here goes dark rather than failing: the config still loads, the tab
    /// still draws, and there is simply no icon with nothing to say why. The
    /// var is still named after the command's old name for that same reason.
    #[test]
    fn the_tab_mark_is_the_escape_wezterm_reads() {
        assert_eq!(
            var_escape(MARK_ON),
            "\u{1b}]1337;SetUserVar=gh_tui=MQ==\u{7}"
        );
        assert_eq!(
            var_escape(MARK_OFF),
            "\u{1b}]1337;SetUserVar=gh_tui=MA==\u{7}"
        );
    }

    /// What the opener inherits, asked of the child itself: each of its three
    /// streams has to be `/dev/null`, or its output is drawn over the screen.
    /// Joining the returned thread is also the proof the child was waited
    /// for -- the file is only complete once it has exited.
    ///
    /// Linux only, because `/proc` is how a process is asked what its
    /// descriptors are. `$$` rather than `self`, since it is the shell's
    /// streams that matter and not `readlink`'s.
    ///
    /// The answers are collected first and written afterwards. Appending each
    /// one with `>> file` passed under bash and failed under dash, which is
    /// `sh` on the CI runner: there the shell itself reported the file as its
    /// stdout while the redirected command ran.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_spawned_opener_shares_no_stream_with_the_terminal() {
        let seen = std::env::temp_dir().join(format!("github-opener-{}", std::process::id()));
        let _ = std::fs::remove_file(&seen);
        let reaper = spawn_apart(
            std::process::Command::new("sh")
                .arg("-c")
                .arg(
                    r#"seen=$(for fd in 0 1 2; do readlink /proc/$$/fd/$fd; done)
                    printf '%s\n' "$seen" > "$0""#,
                )
                .arg(&seen),
        )
        .expect("sh runs");
        reaper.join().expect("the child was waited for");
        let streams = std::fs::read_to_string(&seen).expect("the child wrote what it saw");
        let _ = std::fs::remove_file(&seen);
        assert_eq!(streams, "/dev/null\n/dev/null\n/dev/null\n");
    }

    /// A missing opener is reported, not swallowed: the caller shows it.
    #[test]
    fn a_command_that_cannot_start_is_an_error() {
        assert!(spawn_apart(&mut std::process::Command::new("github-tui-no-such-opener")).is_err());
    }
}
