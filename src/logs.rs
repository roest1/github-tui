//! One job's log: cutting one step out of it, and colouring what comes back.
//!
//! This is the most hard-won logic in the screen it serves, and the only part
//! of the port with its tests written before its implementation. The reason is
//! the failure mode rather than the difficulty: a slice that is off by a step
//! boundary returns plausible log output for the wrong step, and nothing about
//! the result looks wrong. A subtle regression here would be believed.
//!
//! ## Why a log has to be cut up at all
//!
//! GitHub does not serve per-step logs. The run archive is one file per JOB
//! now, which is why `gh run view --log` prints `UNKNOWN STEP` against lines it
//! cannot attribute. So a step's output is reconstructed from the job's log in
//! three passes, each narrowing the last:
//!
//!   1. **the window** — lines whose timestamp falls inside the step's
//!      `[started_at, completed_at]`. Second precision only, so a sub-second
//!      step shares its second with its neighbours and this window can hold
//!      three steps' worth of output. It is a bound, not an answer.
//!   2. **the anchor** — the step's own `##[group]Run …` line inside that
//!      window, preferring one whose text is exactly the step's name. That
//!      preference is what makes `uses:` steps work: an unnamed step is called
//!      `Run actions/checkout@v4` and opens a group of precisely that name.
//!   3. **the cut** — the last `##[error]` before the next step's marker, since
//!      a failed step always ends on one. Failing that, the line before the
//!      next step's marker; failing that, the end of the window.
//!
//! Steps that never open a `Run` group at all — `Set up job`, `Post …`,
//! `Stop containers` — have no anchor, so they get the whole window. That is
//! the honest answer for them rather than a fallback: their output genuinely is
//! everything in their time range.
//!
//! Ported from `_ghw_step_slice` in `bash/bash_github_tui`, line for line
//! deliberately. The awk original compares timestamps as STRINGS, and this
//! does too: ISO 8601 with a fixed-width prefix sorts lexicographically, so
//! parsing them into instants would be slower and no more correct.

/// The marker a step's own log group opens with.
const RUN_GROUP: &str = "Z ##[group]Run ";

/// Lines that mean "the next step has started". The three besides a new `Run`
/// group are the post-job phases, which do not open one.
const NEXT_STEP: &[&str] = &[
    RUN_GROUP,
    "Z Post job cleanup.",
    "Z Cleaning up orphan processes",
    "Z Stop and remove container",
];

/// A log line's timestamp, to second precision.
///
/// The first 19 characters, which is `2026-08-25T19:30:42` — deliberately
/// dropping the fractional part the API sends, because the step boundaries we
/// compare against are themselves only second-accurate. Short lines return
/// what they have, as awk's `substr` does, rather than being skipped.
fn stamp(line: &str) -> &str {
    let end = line
        .char_indices()
        .nth(19)
        .map(|(i, _)| i)
        .unwrap_or(line.len());
    &line[..end]
}

/// A UTF-8 BOM, which GitHub puts at the head of a log file and nowhere else.
/// Stripped per line rather than once, because it has also turned up after a
/// chunk boundary.
fn strip_bom(line: &str) -> &str {
    line.strip_prefix('\u{feff}').unwrap_or(line)
}

/// The text a `##[group]` line opens, with its timestamp removed.
///
/// `^[^ ]+ ##\[group\]` in the original: one unbroken token, a space, then the
/// marker. Anchored, so a line merely containing the marker later does not
/// masquerade as a group header.
fn group_title(line: &str) -> Option<&str> {
    let (first, rest) = line.split_once(' ')?;
    if first.is_empty() {
        return None;
    }
    rest.strip_prefix("##[group]")
}

/// The lines of `log` belonging to the step that ran between `start` and `end`.
///
/// `end` may be empty for a step still in progress: the window then runs to the
/// end of the log, which is the right answer while output is still arriving.
pub fn step_slice<'a>(log: &'a str, start: &str, end: &str, name: &str) -> Vec<&'a str> {
    let (s, e) = (stamp(start), stamp(end));

    // ── 1. the window ──
    let mut win: Vec<&str> = Vec::new();
    for raw in log.lines() {
        let line = strip_bom(raw);
        let ts = stamp(line);
        if ts < s {
            continue;
        }
        // The log is in timestamp order, so the first line past the end is the
        // end -- scanning the remainder of a 15KB log to find nothing is pure
        // cost.
        if !e.is_empty() && ts > e {
            break;
        }
        win.push(line);
    }
    if win.is_empty() {
        return Vec::new();
    }

    // ── 2. the anchor ──
    let mut first_group: Option<usize> = None;
    let mut exact: Option<usize> = None;
    for (i, line) in win.iter().enumerate() {
        if !line.contains(RUN_GROUP) {
            continue;
        }
        if first_group.is_none() {
            first_group = Some(i);
        }
        if group_title(line) == Some(name) {
            exact = Some(i);
            break;
        }
    }
    // Preferring the exact match over the first group is the whole point: a
    // window can open with the TAIL of the previous step's group.
    let a = exact.or(first_group).unwrap_or(0);

    // ── 3. the cut ──
    let next = win
        .iter()
        .enumerate()
        .skip(a + 1)
        .find(|(_, line)| NEXT_STEP.iter().any(|m| line.contains(m)))
        .map(|(i, _)| i);
    let mut z = match next {
        Some(i) => i - 1,
        None => win.len() - 1,
    };
    // A failed step ends on its last error, which is nearly always well short
    // of the next step's marker -- everything between is the runner winding
    // down, and showing it buries the line you came to read. Searched from the
    // END, since the last match is the one wanted.
    if let Some(last) = (a..=z).rev().find(|&i| win[i].contains("Z ##[error]")) {
        z = last;
    }
    win[a..=z].to_vec()
}

/// Strip a leading `2026-08-25T19:30:42.1234567Z `, if there is one.
///
/// Hand-rolled rather than a regex crate, and the shape is exactly the
/// original's `^[0-9-]+T[0-9:.]+Z ?`: anything that does not match is left
/// alone, because a log line that happens not to carry a timestamp is still a
/// log line.
fn strip_stamp(line: &str) -> &str {
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'-') {
        i += 1;
    }
    if i == 0 || b.get(i) != Some(&b'T') {
        return line;
    }
    let mut j = i + 1;
    while j < b.len() && (b[j].is_ascii_digit() || b[j] == b':' || b[j] == b'.') {
        j += 1;
    }
    if j == i + 1 || b.get(j) != Some(&b'Z') {
        return line;
    }
    let k = j + 1;
    let k = if b.get(k) == Some(&b' ') { k + 1 } else { k };
    &line[k..]
}

/// Columns between tab stops, which is what a terminal uses unless told
/// otherwise.
const TAB: usize = 8;

/// What kind of text is being let through to the terminal.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Text {
    /// A log line: its colours survive and its tabs keep their columns.
    Log,
    /// A name -- a job, a step, a title. Characters and nothing else.
    Name,
}

/// The one gate between text somebody else wrote and the terminal.
///
/// A log is the output of code this user did not write: a pull request from a
/// fork runs its own workflow, and whatever that prints is fetched with its
/// escape sequences intact so cargo's colours arrive as cargo's colours.
/// Passed through as it came, `ESC ] 52 ; c ; <base64> BEL` in a test's
/// output replaced the clipboard of whoever selected the run, `ESC [ 2 J`
/// blanked the screen, and a query sequence had the terminal type its reply
/// into this program as keystrokes. `tui-core` measures and truncates a line;
/// it does not read it.
///
/// So this is an ALLOWLIST, and it has one entry: a colour, `ESC [ … m` with
/// nothing between but digits, `;` and `:`. Everything else that is not a
/// printable character is dropped -- every other CSI (a private one such as
/// `ESC [ > 4 ; 2 m` ends in `m` and is not a colour), every OSC, DCS, APC, PM
/// and SOS with its payload, every two-character escape, and every C0 and C1
/// control, the carriage return among them. Naming the dangerous sequences
/// instead would be a list that is complete until a terminal adds one.
///
/// What comes out therefore holds no control character but `ESC`, and every
/// `ESC` in it opens a whole colour sequence. The tests assert exactly that,
/// over every short string of the characters a sequence can be built from.
fn scrub(text: &str, kind: Text) -> String {
    let mut out = String::with_capacity(text.len());
    // Cells written so far, which is what a tab stop is measured in.
    let mut col = 0;
    // Whether a colour is still in force at the end of the line.
    let mut open = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                if let Some(colour) = escape(&mut chars) {
                    if kind == Text::Log {
                        open = colour != "\x1b[0m" && colour != "\x1b[m";
                        out.push_str(&colour);
                    }
                }
            }
            // A tab is one character and up to eight cells, and the terminal
            // counts those cells from the edge of the SCREEN rather than of
            // the pane -- so left in place it is measured as one cell, drawn
            // as several, and the end of the line lands past the pane's edge.
            '\t' if kind == Text::Log => {
                let n = TAB - col % TAB;
                out.extend(std::iter::repeat_n(' ', n));
                col += n;
            }
            // A name is one line whatever it was handed.
            '\t' | '\n' if kind == Text::Name => {
                out.push(' ');
                col += 1;
            }
            c if c.is_control() => {}
            c => {
                out.push(c);
                col += tui_core::width::char_width(c);
            }
        }
    }
    // A colour left open would run on into whatever is drawn next, which is
    // the divider, the row list and the footer. A log may colour its own
    // line; it does not get to colour the screen.
    if open {
        out.push_str(crate::paint::OFF);
    }
    out
}

/// Read the escape sequence whose `ESC` was just consumed, and hand it back
/// only if it is a colour.
///
/// The sequence is consumed whole either way, payload included, so dropping
/// one does not leave its insides behind as text. One cut short -- by the end
/// of the line, or by a character that cannot belong to it -- is dropped as
/// far as it got, and what follows is read as whatever it is.
fn escape(chars: &mut std::iter::Peekable<std::str::Chars>) -> Option<String> {
    match chars.peek().copied()? {
        // CSI: parameter bytes, then intermediate bytes, then one final byte.
        '[' => {
            chars.next();
            let mut seq = String::from("\x1b[");
            // A colour's parameters are numbers and their separators. `<`,
            // `=`, `>` and `?` are parameter bytes too, and mark the sequence
            // as private: something else that happens to end in `m`.
            let mut numeric = true;
            while let Some(c) = chars.next_if(|c| ('0'..='?').contains(c)) {
                numeric &= c.is_ascii_digit() || c == ';' || c == ':';
                seq.push(c);
            }
            let mut bare = true;
            while chars.next_if(|c| (' '..='/').contains(c)).is_some() {
                bare = false;
            }
            let last = chars.next_if(|c| ('@'..='~').contains(c))?;
            (last == 'm' && numeric && bare).then(|| {
                seq.push('m');
                seq
            })
        }
        // OSC, DCS, SOS, PM and APC: a string that runs to its terminator.
        // The clipboard, the window title, hyperlinks, inline images and
        // every query whose answer is typed back are all in here.
        ']' | 'P' | 'X' | '^' | '_' => {
            chars.next();
            string_body(chars);
            None
        }
        // An escape with intermediate bytes, such as the ones that switch
        // character set: the intermediates and the byte that ends them.
        ' '..='/' => {
            while chars.next_if(|c| (' '..='/').contains(c)).is_some() {}
            chars.next_if(|c| ('0'..='~').contains(c));
            None
        }
        // Every other escape is two characters: a full reset, a cursor save,
        // a reverse line feed.
        '0'..='~' => {
            chars.next();
            None
        }
        // `ESC` and then something that continues no sequence. The `ESC` is
        // already gone; the next character is read on its own.
        _ => None,
    }
}

/// Consume a string sequence's payload, through its terminator.
///
/// `BEL` or `ESC \` ends one, and so does the single-character form of the
/// latter. With no terminator on the line the rest of the line is the
/// payload, which is how a terminal would have read it.
fn string_body(chars: &mut std::iter::Peekable<std::str::Chars>) {
    while let Some(c) = chars.peek().copied() {
        match c {
            '\x07' | '\u{9c}' => {
                chars.next();
                return;
            }
            '\x1b' => {
                // Any escape ends the string. Only `ESC \` is part of it; a
                // different one is left to be read as itself.
                let mut ahead = chars.clone();
                ahead.next();
                if ahead.peek() == Some(&'\\') {
                    chars.next();
                    chars.next();
                }
                return;
            }
            _ => {
                chars.next();
            }
        }
    }
}

/// One line of somebody else's output, made safe to draw: colours kept, tabs
/// expanded, every other control sequence and character dropped. See `scrub`.
///
/// Also what a screen passes its finished pane lines through, since those
/// carry names the API handed over as well as this crate's own colours.
pub fn clean(line: &str) -> String {
    scrub(line, Text::Log)
}

/// A name from the API, as plain characters on one line.
///
/// A job's and a step's name come from the workflow file, which on a pull
/// request from a fork is the fork's. No colours here: a row's label is
/// styled by the list, and a name has no business restyling it.
pub fn plain(name: &str) -> String {
    scrub(name, Text::Name)
}

/// GitHub's log markup, rendered.
///
/// `None` for `##[endgroup]`, which exists to close a group and says nothing to
/// a reader. The colours of the tool being run pass through -- the pane
/// renders ANSI, so a cargo error arrives in cargo's own colours rather than
/// in a reimplementation of them -- and nothing else of what it emitted does.
pub fn fmt_line(raw: &str) -> Option<String> {
    use crate::paint::{BLU, BOLD, DIM, OFF, RED, YEL};
    let line = strip_bom(raw);
    let line = strip_stamp(line);
    // Before the markers are looked for, so nothing below this line handles
    // text that has not been through it. It also takes the carriage returns:
    // the one a Windows runner ends every line with, and the ones a progress
    // bar puts mid-line, each of which would send the cursor to column zero
    // of the SCREEN and draw the rest of the line over the row list.
    let line = clean(line);
    let line = line.as_str();

    if line.starts_with("##[endgroup]") {
        return None;
    }
    for (marker, prefix) in [
        ("##[error]", format!("{BOLD}{RED}\u{2716} ")),
        ("##[warning]", format!("{YEL}\u{26a0} ")),
        ("##[group]", format!("{BLU}\u{25b8} ")),
    ] {
        if let Some(rest) = line.strip_prefix(marker) {
            return Some(format!("{prefix}{rest}{OFF}"));
        }
    }
    for marker in ["##[notice]", "##[debug]", "##[command]", "##[section]"] {
        if let Some(rest) = line.strip_prefix(marker) {
            return Some(format!("{DIM}{rest}{OFF}"));
        }
    }
    Some(line.to_string())
}

/// A whole slice, formatted, with the closing markers dropped.
pub fn fmt_log<'a>(lines: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    lines.into_iter().filter_map(fmt_line).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A two-step job log in the real shape: timestamped lines, `##[group]Run`
    /// headers naming each step, an `##[error]` in the failing one, and the
    /// post-job phases after.
    const LOG: &str = concat!(
        "\u{feff}2026-08-25T19:30:40.0000000Z ##[group]Run actions/checkout@v4\n",
        "2026-08-25T19:30:40.1000000Z with:\n",
        "2026-08-25T19:30:40.2000000Z ##[endgroup]\n",
        "2026-08-25T19:30:41.0000000Z Syncing repository\n",
        "2026-08-25T19:30:42.0000000Z ##[group]Run cargo test --all\n",
        "2026-08-25T19:30:43.0000000Z    Compiling github v0.1.0\n",
        "2026-08-25T19:30:44.0000000Z ##[error]test slicing failed\n",
        "2026-08-25T19:30:45.0000000Z leaving the runner\n",
        "2026-08-25T19:30:46.0000000Z Post job cleanup.\n",
        "2026-08-25T19:30:47.0000000Z Cleaning up orphan processes\n",
    );

    #[test]
    fn it_cuts_at_the_step_whose_name_matches_the_group() {
        let got = step_slice(
            LOG,
            "2026-08-25T19:30:42Z",
            "2026-08-25T19:30:45Z",
            "Run cargo test --all",
        );
        assert!(got[0].contains("##[group]Run cargo test --all"), "{got:?}");
        // Cut at the last error, not at the post-job marker: the line after it
        // is the runner winding down.
        assert!(got.last().unwrap().contains("##[error]test slicing failed"));
        assert!(!got.iter().any(|l| l.contains("leaving the runner")));
    }

    /// The case the second pass exists for. A window opened one second early
    /// holds the tail of the PREVIOUS step, including its group header, so
    /// taking the first group found would return the wrong step's output.
    #[test]
    fn a_window_holding_two_groups_picks_the_one_it_was_asked_for() {
        let got = step_slice(
            LOG,
            "2026-08-25T19:30:40Z",
            "2026-08-25T19:30:45Z",
            "Run cargo test --all",
        );
        assert!(got[0].contains("Run cargo test --all"), "{got:?}");
        assert!(!got.iter().any(|l| l.contains("Syncing repository")));
    }

    /// An unnamed `uses:` step is named after what it runs, and its group says
    /// the same thing -- which is the whole reason the exact-name preference
    /// works at all.
    #[test]
    fn an_unnamed_uses_step_matches_on_its_own_name() {
        let got = step_slice(
            LOG,
            "2026-08-25T19:30:40Z",
            "2026-08-25T19:30:41Z",
            "Run actions/checkout@v4",
        );
        assert!(got[0].contains("Run actions/checkout@v4"));
        assert!(got.iter().any(|l| l.contains("Syncing repository")));
        assert!(!got.iter().any(|l| l.contains("cargo test")));
    }

    /// `Set up job` and the post phases never open a Run group, so they have no
    /// anchor. The whole window is the honest answer, not a failure.
    #[test]
    fn a_step_with_no_group_gets_its_whole_window() {
        let log = concat!(
            "2026-08-25T19:30:10.0000000Z Current runner version: '2.300.0'\n",
            "2026-08-25T19:30:11.0000000Z Operating System\n",
        );
        let got = step_slice(
            log,
            "2026-08-25T19:30:10Z",
            "2026-08-25T19:30:11Z",
            "Set up job",
        );
        assert_eq!(got.len(), 2);
    }

    /// A step still running has no `completed_at`. The window has to stay open
    /// to the end of the log, or a live step shows nothing while it is the one
    /// thing you are watching.
    #[test]
    fn a_running_step_slices_to_the_end_of_what_has_arrived() {
        let got = step_slice(LOG, "2026-08-25T19:30:42Z", "", "Run cargo test --all");
        assert!(got.iter().any(|l| l.contains("Compiling github")));
    }

    #[test]
    fn a_window_that_matches_nothing_is_empty_rather_than_everything() {
        assert!(step_slice(LOG, "2027-01-01T00:00:00Z", "2027-01-02T00:00:00Z", "x").is_empty());
        assert!(step_slice("", "2026-08-25T19:30:42Z", "", "x").is_empty());
    }

    /// The BOM is on the first line of every log GitHub serves. Left in place
    /// it defeats the `##[group]` prefix match on exactly the line that matters
    /// most -- the first step's header.
    #[test]
    fn the_leading_bom_does_not_hide_the_first_group() {
        let got = step_slice(
            LOG,
            "2026-08-25T19:30:40Z",
            "2026-08-25T19:30:41Z",
            "Run actions/checkout@v4",
        );
        assert!(group_title(got[0]).is_some(), "{:?}", got[0]);
    }

    #[test]
    fn formatting_drops_the_timestamp_and_the_closing_marker() {
        assert_eq!(
            fmt_line("2026-08-25T19:30:40.0000000Z hello").as_deref(),
            Some("hello")
        );
        assert_eq!(fmt_line("2026-08-25T19:30:40.0000000Z ##[endgroup]"), None);
        let err = fmt_line("2026-08-25T19:30:44.0000000Z ##[error]boom").unwrap();
        assert!(err.contains("boom") && err.contains("\u{2716}") && !err.contains("##["));
    }

    /// A line without a timestamp is still a line. The original's regex simply
    /// does not match, and anything that reached for `[..k]` unconditionally
    /// would eat the first characters of real output.
    #[test]
    fn a_line_with_no_timestamp_survives_intact() {
        assert_eq!(fmt_line("plain output").as_deref(), Some("plain output"));
        assert_eq!(
            fmt_line("12-34 not a stamp").as_deref(),
            Some("12-34 not a stamp")
        );
    }

    /// Windows runners emit CRLF. A trailing CR left in place puts the cursor
    /// back at column zero and the NEXT pane line overwrites this one.
    #[test]
    fn a_carriage_return_does_not_survive_into_the_pane() {
        assert_eq!(
            fmt_line("2026-08-25T19:30:40.0000000Z done\r").as_deref(),
            Some("done")
        );
    }

    /// And a bare one mid-line, which is what a progress bar prints, sends
    /// the cursor to column zero of the SCREEN: the rest of the line would be
    /// drawn over the row list.
    #[test]
    fn a_carriage_return_inside_a_line_is_dropped_too() {
        let got = fmt_line("2026-08-25T19:30:40.0000000Z fetch 10%\rfetch 99%\r").unwrap();
        assert_eq!(got, "fetch 10%fetch 99%");
    }

    const STAMP: &str = "2026-08-25T19:30:40.0000000Z ";

    fn shown(body: &str) -> String {
        fmt_line(&format!("{STAMP}{body}")).expect("a line")
    }

    /// The promise `scrub` makes, checked from the outside: no control
    /// character but `ESC`, and every `ESC` opens `[`, digits and separators,
    /// `m` -- a colour and nothing else.
    fn only_colour_survives(s: &str) -> bool {
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                if chars.next() != Some('[') {
                    return false;
                }
                loop {
                    match chars.next() {
                        Some('m') => break,
                        Some(c) if c.is_ascii_digit() || c == ';' || c == ':' => {}
                        _ => return false,
                    }
                }
            } else if c.is_control() {
                return false;
            }
        }
        true
    }

    /// The attack this gate exists for. A test in a fork's pull request
    /// prints one line, and whoever selects the failed run has their
    /// clipboard replaced -- with a command, ready for the next paste into a
    /// shell. The payload goes with the sequence: left behind as text it
    /// would be noise, and it is not output anyone wrote to be read.
    #[test]
    fn a_log_cannot_set_the_clipboard() {
        // Ended by BEL, by ESC \, and by the one-character form of that.
        for end in ["\x07", "\x1b\\", "\u{9c}"] {
            let got = shown(&format!("before\x1b]52;c;Y3VybCBldmlsIHwgc2g={end}after"));
            assert_eq!(got, "beforeafter", "terminated by {end:?}");
        }
        // Never ended at all: the rest of the line is its payload, as it
        // would have been to the terminal.
        assert_eq!(shown("before\x1b]52;c;Y3VybCBldmlsIHwgc2g="), "before");
        // And through a marker, which formats the text after it.
        let err = shown("##[error]\x1b]52;c;Y3VybA==\x07boom");
        assert!(err.contains("boom"), "{err:?}");
        assert!(
            !err.contains("52;c") && only_colour_survives(&err),
            "{err:?}"
        );
    }

    /// Clearing the screen, moving the cursor, switching screen buffer, and
    /// the queries a terminal answers by TYPING -- every one a CSI, none a
    /// colour. Two of them end in `m`, which is why the test for a colour is
    /// its whole shape and not its last letter.
    #[test]
    fn no_csi_but_a_colour_gets_through() {
        for seq in [
            "\x1b[2J",       // erase the screen
            "\x1b[H",        // cursor home
            "\x1b[10;10H",   // cursor to a cell
            "\x1b[3A",       // cursor up, onto lines already drawn
            "\x1b[K",        // erase the line
            "\x1b[?1049l",   // leave the alternate screen
            "\x1b[?25l",     // hide the cursor
            "\x1b[6n",       // report the cursor position, as keystrokes
            "\x1b[c",        // report what terminal this is, likewise
            "\x1b[>4;2m",    // key reporting mode: private, and ends in m
            "\x1b[?4m",      // the query for the same
            "\x1b[1 q",      // cursor shape, with an intermediate byte
            "\x1b[!p",       // soft reset
            "\x1b[21;9999t", // window title report
        ] {
            assert_eq!(shown(&format!("a{seq}b")), "ab", "{seq:?} got through");
        }
    }

    /// The other half of the trade: the colours a tool gave its own output
    /// arrive as it sent them. Losing them would be the cheap way to be safe.
    #[test]
    fn a_colour_survives_untouched() {
        for line in [
            "\x1b[31merror\x1b[0m: mismatched types",
            "\x1b[1m\x1b[38;5;203mFAILED\x1b[0m tests::it_works",
            "\x1b[38;2;255;100;0mtruecolour\x1b[m",
            "\x1b[38:2::255:100:0mwith colons\x1b[0m",
            "\x1b[0;1;4mbold underline\x1b[0m",
        ] {
            assert_eq!(shown(line), line);
            assert!(only_colour_survives(&shown(line)));
        }
    }

    /// A colour a line leaves open is closed for it. Otherwise the last log
    /// line in the pane decides what colour the row list and the footer are.
    #[test]
    fn an_unclosed_colour_stops_at_the_end_of_its_line() {
        assert_eq!(
            shown("\x1b[31mred to the end"),
            "\x1b[31mred to the end\x1b[0m"
        );
        // Already closed, so nothing is added.
        assert_eq!(shown("\x1b[31mred\x1b[0m"), "\x1b[31mred\x1b[0m");
        assert_eq!(shown("no colour at all"), "no colour at all");
    }

    /// A tab is one character and up to eight cells, so it is measured as one
    /// and drawn as several -- and the end of the line lands outside the
    /// pane. Expanded to its stop, in CELLS: a wide glyph before it is two,
    /// and a colour sequence is none.
    #[test]
    fn a_tab_becomes_the_spaces_to_its_stop() {
        assert_eq!(shown("a\tb"), format!("a{}b", " ".repeat(7)));
        assert_eq!(shown("\tb"), format!("{}b", " ".repeat(8)));
        assert_eq!(shown("12345678\tb"), format!("12345678{}b", " ".repeat(8)));
        assert_eq!(shown("\u{2705}\tb"), format!("\u{2705}{}b", " ".repeat(6)));
        assert_eq!(
            shown("\x1b[31mab\x1b[0m\tc"),
            format!("\x1b[31mab\x1b[0m{}c", " ".repeat(6))
        );
        assert!(!shown("a\tb\tc").contains('\t'));
    }

    /// The single characters that act on a terminal without any escape:
    /// backspace overprints what was just written, a form feed clears some
    /// terminals, and shift-out swaps the character set for everything after.
    #[test]
    fn a_lone_control_character_is_dropped() {
        for c in [
            '\0', '\x07', '\x08', '\x0b', '\x0c', '\x0e', '\x0f', '\x18', '\x7f',
            // The C1 block: CSI and OSC as single characters, to a terminal
            // that reads them that way.
            '\u{9b}', '\u{9d}', '\u{90}',
        ] {
            assert_eq!(shown(&format!("a{c}b")), "ab", "{c:?} got through");
        }
    }

    /// Escapes that are neither CSI nor a string: a full terminal reset, the
    /// cursor save and restore, and the character-set switch that turns
    /// every letter after it into line-drawing.
    #[test]
    fn a_short_escape_goes_whole() {
        for seq in [
            "\x1bc", "\x1b7", "\x1b8", "\x1bM", "\x1b=", "\x1b(0", "\x1b#8",
        ] {
            assert_eq!(shown(&format!("a{seq}b")), "ab", "{seq:?} got through");
        }
    }

    /// The other string sequences, payload and all: a capability query whose
    /// answer is typed back, and an inline image.
    #[test]
    fn a_device_string_goes_with_its_payload() {
        assert_eq!(shown("a\x1bP+q544e\x1b\\b"), "ab");
        assert_eq!(shown("a\x1b_Gf=100;AAAA\x1b\\b"), "ab");
        assert_eq!(shown("a\x1b^private\x1b\\b"), "ab");
        // A hyperlink keeps its text and loses its target.
        assert_eq!(
            shown("see \x1b]8;;https://example.com\x1b\\the docs\x1b]8;;\x1b\\."),
            "see the docs."
        );
    }

    /// A sequence the line ends in the middle of, or that is interrupted by
    /// another, must not leave an `ESC` behind for the next thing drawn to
    /// complete.
    #[test]
    fn a_sequence_cut_short_leaves_nothing_behind() {
        assert_eq!(shown("abc\x1b"), "abc");
        assert_eq!(shown("abc\x1b["), "abc");
        assert_eq!(shown("abc\x1b[31"), "abc");
        assert_eq!(shown("abc\x1b]0;title"), "abc");
        // An escape inside a string ends it and is read as itself, so the
        // colour after an unterminated title still colours.
        assert_eq!(
            shown("\x1b]0;title\x1b[32mgreen\x1b[0m"),
            "\x1b[32mgreen\x1b[0m"
        );
        // A control character inside a CSI is executed by a terminal and the
        // sequence carries on; here both halves are simply gone or inert.
        let got = shown("a\x1b[3\x071mb");
        assert!(only_colour_survives(&got), "{got:?}");
        assert!(!got.contains('\x1b'), "{got:?}");
    }

    /// Every string of up to five of the characters a sequence can be built
    /// from. The examples above are the ones somebody thought of; this is
    /// the rest, and it is what makes "nothing but a colour" a property of
    /// the gate and not of the examples.
    #[test]
    fn nothing_but_a_colour_survives_any_short_input() {
        const ALPHABET: [char; 14] = [
            '\x1b', '[', ']', 'm', '1', ';', '?', ' ', '\x07', '\\', 'P', 'x', '\r', '\u{9b}',
        ];
        let n = ALPHABET.len();
        let mut tried = 0u32;
        for len in 0..=5u32 {
            for code in 0..n.pow(len) {
                let mut code = code;
                let mut s = String::new();
                for _ in 0..len {
                    s.push(ALPHABET[code % n]);
                    code /= n;
                }
                let got = clean(&s);
                assert!(only_colour_survives(&got), "{s:?} became {got:?}");
                // Cleaning what is already clean changes nothing, so a pane
                // line that goes through twice is the line that went once.
                assert_eq!(clean(&got), got, "{s:?} is not stable");
                assert!(only_colour_survives(&plain(&s)) && !plain(&s).contains('\x1b'));
                tried += 1;
            }
        }
        assert!(tried > 500_000);
    }

    /// The pager is handed the same lines, so it is the same gate.
    #[test]
    fn a_whole_log_is_cleaned_line_by_line() {
        let log = format!(
            "{STAMP}one\x1b[2J\n{STAMP}##[endgroup]\n{STAMP}\x1b]52;c;QQ==\x07two\ttab\r\n"
        );
        let got = fmt_log(log.lines());
        assert_eq!(
            got,
            vec!["one".to_string(), format!("two{}tab", " ".repeat(5))]
        );
    }

    /// A name is characters on one line: no colour either, because a row's
    /// label is styled by the list, and a job name comes from a workflow
    /// file that a fork's pull request brings with it.
    #[test]
    fn a_name_keeps_nothing_but_its_characters() {
        assert_eq!(plain("build\x1b]52;c;QQ==\x07 (ubuntu)"), "build (ubuntu)");
        assert_eq!(plain("\x1b[31mtest\x1b[0m"), "test");
        assert_eq!(plain("one\ttwo\nthree\r"), "one two three");
        assert_eq!(plain("caf\u{e9} \u{2705}"), "caf\u{e9} \u{2705}");
    }
}
