# CLAUDE.md — github-tui

`github`: your repos, then everything about one, as a terminal UI. Built on
[roest1/tui-core](https://github.com/roest1/tui-core) and pinned in
[roest1/dotfiles](https://github.com/roest1/dotfiles) as a submodule at `tui/github`. Split out
of dotfiles on 2026-11-23 with its history. The entries below moved from dotfiles'
`tui/CLAUDE.md` as they were; trim them here. The port's history and the parity table are in
[`docs/decisions/`](docs/decisions/).

## Layout

- `src/` — the port. `branches` (the full-width branch table), `menu` (a
  row's actions — the design's three-dots), `user` (the user level — the root, and its Account and
  Keys tabs), `orgs` (the org list and one org's three tabs), `index` (every
  clone under `~/github`, read from disk), `root` (the repo tree — the user
  level's Repos tab), `runs` (the Actions screen), `steps` (one run as a tree),
  `gh` (the threaded `gh api` caller and the cross-session cache), `scopes`
  (what the token may do, asked of the token), `model` (the API's types and the
  one place that says what a status MEANS), `render` (the pane bodies), `logs`
  (step slicing and log colouring), `time` (ISO 8601 without a date crate),
  `paint` (the palette), plus the lib/bin pair. **Not installed yet** — see
  below.

## Things not to undo

- **A placeholder row is a ROW, and `at()` has to count it.** The Environments
  screen draws "no environments yet" when the list is empty, so the rows below
  it sit where a single environment would have put them — and reading the
  position off `envs.len()` alone put `+ new environment` one row too high, on
  exactly the repo where creating one is the only thing the screen can do.
  `enter` on it did nothing.

  Same class as the `dots` console's conditional rows, and the same answer: the
  `At` walk and `rows()` are two readings of one list, so every screen with
  either gets a test asserting they agree — including in the empty shape, which
  is the one the first version of both got wrong.

- **A menu item that changes something asks first, and "the tool reports" is
  not the same as "the tool asks".** That conflation was a real hole, wider
  than it looked: `git push --delete` prints what it did and fails loudly on a
  protected branch, and `gh api -X POST .../rename` does neither — so deleting
  a remote branch was ONE `enter` from a menu row, behind a note claiming the
  tool confirmed it.

  `Item::confirm()` asks; `danger()` implies it and adds "cannot be undone".
  Where the tool genuinely does stop and ask — `gh pr create` for a title, `gh
  auth refresh` printing a code and waiting — neither flag is set, so there is
  still only ever one prompt.

  **The confirm comes AFTER the field**, so what you approve is the command
  with the name you just typed in it rather than a template. Only `y` acts; any
  other key withdraws, and so does moving the cursor. `focus` clears it
  unconditionally rather than only on a change — `app` calls `focus` only when
  the cursor moved, so the guarded version was equivalent with a subtlety to
  reason about, and that subtlety is the sort of thing that leaves a delete
  armed.

  Because this lives in `Menu`, every screen's menu has it. That is the shared
  confirm the decision doc kept listing as open.

  The first version returned early with `continue` after handling a field,
  which silently dropped a `Flow::Push` or `Flow::Pop` from `answered` — a
  screen asking for a name and then opening something with it did nothing. It
  is an `if/else` so control still reaches the bottom of the loop.

- **No emoji in a fixed-width column, and the branch marker is where that bit.**
  The design asks for a lock glyph on protected branches. `U+1F512` is East
  Asian Wide — two cells in wezterm, one in terminals that disagree — in the one
  column whose entire job is being fixed-width. `★` and `●` are single-width
  and already used here; protection is a `prot` text column on the right.
  `branches::tests::the_marker_column_is_fixed_width` asserts every marker is
  inside the BMP, and it also counts the prefix in CHARACTERS rather than
  bytes: `str::find` returns a byte offset and `★` is three bytes, so the byte
  version failed on a perfectly aligned row and would have passed on a broken
  one whose counts cancelled.

- **Every control is a dropdown, and bulk actions are a menu.** The PR mock
  draws state toggles, a sort order and a search in a header strip and five
  filters down the side. They are all the same thing — a named thing you open,
  pick from and close — so they are all sidebar rows, one control per row, and
  `enter` means exactly one thing. A header row holding four controls side by
  side would need column focus inside a row, which is the axis this does not
  have.

  `space` ticks a pull request, the key that toggles a section in `dots`. With
  anything ticked the list grows a row saying how many, and `enter` on it opens
  a `Menu` whose items act on the set — so bulk actions cost no new region and
  no new keys, and inherit the confirm.

  **Ticks are kept by NUMBER and dropdown picks by NAME**, never by position. A
  refresh reorders the list and grows the option lists, and a `Vec<bool>`
  indexed by position would silently move a tick onto a different pull request
  or a filter onto a different person.

  Only `state` refetches, because it is the only control that changes which
  pull requests EXIST. Author, assignee, involvement, labels, sort and the `/`
  search are all applied to the fifty that came back.

- **`enter` on a row opens that row's MENU, and that is the design's
  three-dots.** It needs nothing from `tui-core` — a menu is a list of rows and
  `Flow::Push` opens it. What it avoids is the only expensive part of drawing
  buttons on a row: putting the cursor ON one needs column-wise focus, a second
  navigation axis, and it collides with `h`/`l` meaning close/open everywhere
  else. The bash implementation already worked this way (thirteen actions on a
  PR row, seven on a branch), so this is the shape being ported.

  **Most items are `Flow::Detach`, and that is the finding worth keeping.**
  `gh pr create` prompts for a title and body on a tty; `gh pr edit` prompts for
  what to change; `git push --delete` prints what it did and fails loudly on a
  protected branch. Handing them the real terminal buys text entry,
  confirmation and error reporting in the tools' own words — so full
  functionality does NOT wait on a text-input widget. Showing the command in
  the pane IS the confirmation; a destructive item is marked in the row, the
  pane and the footer, and adds no prompt of its own, because the tool already
  asks or already reports and two confirms is one too many.

- **A long command WRAPS in the menu pane; it does not get truncated.**
  `argv.join(" ")` was fine while every item was `gh pr merge 86 --repo x`. An
  environment PUT carries the whole environment and comes to three hundred
  characters, which the pane cut off — and a confirm you cannot read all of is
  not a confirm, since showing the command IS the confirmation here.
  `menu::wrap_argv` breaks it at argument boundaries and keeps a flag with the
  value after it, because `-F` on one line and `reviewers[][id]=7` on the next
  reads as two things.

- **Every environment change sends the WHOLE environment.** `PUT
  /repos/{owner}/{repo}/environments/{name}` is "create or update", and the
  documentation does not say what happens to a parameter you leave out
  (checked, not assumed). The bash version sent `{wait_timer: n}` alone, so IF
  omission clears, setting a wait timer silently dropped the required reviewers
  — and nobody would find out until a deploy that should have waited for an
  approval did not.

  Sending everything makes the question moot: whatever the semantics, the
  environment afterwards is what the pane showed plus the one change. It costs
  nothing, because `gh api`'s field syntax carries all of it with no shell and
  no temp file: `-F 'reviewers[][type]=User' -F 'reviewers[][id]=123'` builds
  an array of objects, one per pair, in order; `-F 'reviewers[]'` sends an
  empty array; `-F key[sub]=value` nests; `-F x=null` sends a real null. All
  four were verified against `gh api --verbose` against a path that could not
  create anything, before any of the screen was written.

- **Required reviewers get a PICKER because the API takes an id.** No amount of
  typing produces a numeric user id, and the collaborators endpoint hands back
  logins with theirs — so a list to tick is both nicer than a field and the
  only shape that can work. It also matches the semantics: the field is a SET
  the PUT replaces wholesale, so it opens pre-ticked with whoever is required
  now, and whoever is required is a candidate whether or not they are a
  collaborator. A team never will be, and an outside reviewer would otherwise
  vanish from the list that replaces them.

  When the collaborators call fails — it needs push, which you do not have on
  someone else's repo — the error is said out loud EVEN THOUGH there are rows,
  because the rows are then only the already-required ones. A short list that
  looks like the whole list is the one way this screen could mislead.

- **A secret's value never enters this process, and no field here could
  improve on that.** `gh secret set NAME` reads it from the terminal with echo
  off and encrypts it locally before it leaves the machine, so `Flow::Detach`
  means the value is never in a `String` here and cannot end up in a redraw.
  The NAME is asked for, because gh takes that as an argument.

  It is passed **as typed**, not upper-cased the way the bash version did:
  GitHub treats secret names case-insensitively, so that was a convention
  rather than a requirement, and silently rewriting what someone typed is not a
  thing to do on their behalf. What IS checked here is the part GitHub will
  refuse — the character set and the reserved `GITHUB_` prefix — because
  learning that from a round trip means typing a name, handing over the
  terminal, typing a value, and only then being told.

- **The scope of a new secret comes from the GROUP the cursor is in.** The
  Secrets screen is the repository's secrets and each environment's as groups,
  so `+ new secret in staging` lives inside the staging group and there is no
  dialog asking where it should live — where the fzf version opened a "Where
  does it live?" menu first. Navigate, do not ask. The groups also mean `/`
  narrows across every scope at once, which answers the question you actually
  arrive with: is `DEPLOY_TOKEN` set anywhere.

- **A command must not depend on the process's working directory.** `github`
  is launched from anywhere — that is what `index` is for — so every `gh` a
  menu offers names the repo (as `--repo`, or inside a `gh api` path) and every
  `git` carries `-C`. One item got this wrong: `New pull request` ran `gh pr
  create --head <branch>` with no `--repo`, which worked when launched from
  inside the clone and failed with `not a git repository` everywhere else.
  `branches::tests::no_menu_item_depends_on_the_working_directory` asserts the
  rule over every item of every branch's menu rather than over the one that
  broke.

- **`rules.rs` is read-only, and that is why it exists while `rulesets` is
  dropped.** The two look like the same feature and are not. The dropped row
  is a ruleset browser and EDITOR whose writes nobody has exercised; this is
  two GETs — `rules/branches/{branch}`, then `rulesets/{id}` per ruleset — so
  there is no write to get wrong and no shape for an unvalidated feature to
  set. The first version of this file argued for dropping both, and the
  distinction is what reversed it.

  **The bypass is the point, not the rule list.** `protected: true` on a
  branch, which the table already shows, answers a weaker question than it
  looks like it does: a ruleset with a bypass actor turns every rule under it
  into guidance somebody can push straight past. So the bypass is in the
  group's DETAIL, readable without selecting, and the actors are in the pane.
  An unreadable ruleset says **bypass unknown**, never "no bypass" — same
  rule as the reviewer picker naming its failed collaborators call, and for
  the same reason: the rules above still apply, so a missing line must not
  read as an absence.

  **An empty answer is said out loud.** No ruleset targeting a branch is the
  normal state for a feature branch, so the screen says "no rules apply" and
  why. A blank screen would read as a finding.

  **The parameters render generically**, with a hand-written summary for only
  the two worth reading at a glance. Not future-proofing for its own sake:
  `copilot_code_review` postdates the bash version and arrived rendering
  correctly with no code. The summary lists what is ON; the pane shows the OFF
  settings too and expands the required-checks array by name.

  **A slash in a branch name needs no encoding, measured.**
  `rules/branches/main/x` answers `[]` rather than 404 on a repo whose `main`
  has four rules, so the route takes the branch as the rest of the path. `#` is
  still escaped — it is legal in a branch name and would start a fragment.

- **The Branches table is local git first.** One `for-each-ref` gives every
  remote branch's age, author, subject AND ahead/behind against the default —
  offline, instantly, in one spawn. The API is asked only for what git cannot
  know: `protected`, open PRs, and checks. So the table draws before any
  request is made. `%(ahead-behind:)` needs git >= 2.41 and an unsupported atom
  fails the WHOLE command, so the fallback re-runs without it rather than
  trying to salvage a partial answer.

  **Checks cost no extra call**: a run carries its `head_branch`, so the newest
  run per branch is that branch's status, and the repo `Host` shares one
  `Fetcher` across its tabs so Branches reuses what Actions asked for.

  **The marker cell has THREE states, in one cell.** `●` you are on it, `○`
  you have it, blank it is only on the remote — which is the question in front
  of "should I check it out", and the one a list of REMOTE branches could not
  answer. It costs one more `for-each-ref` over `refs/heads` and no new
  column; blank is the common case on a long list, which is the right way
  round. `U+25CB` is BMP and single-width, so the fixed-width assertion still
  holds. The menu's Checkout says which act it is about to perform, since
  creating a branch and moving to one are different things.

  **"Your branches" matches on EMAIL.** Measured here: the same person authored
  commits as both `Riley Oest` and `roest1` — a local `user.name` and GitHub's
  web UI disagreeing — while the email was identical on every one. Matching the
  name filed your own branches under someone else.

- **There is ONE root, and it is the user level — including inside a repo.**
  This is the whole answer to "I want my settings even when I opened this in a
  repo", and it is a rejection of the obvious design. Opening at the repo level
  when you are in a repo and the user level when you are not gives you TWO
  roots, and two roots cannot connect: with the repo as the root there is
  nothing above it to walk up to.

  So standing in a repo seeds the SELECTION, not the root. `github` in a repo
  opens the user level on its Repos tab with that repo selected; `q` walks up
  from a workflow run to the repo list to the account and quits there. Do not
  add a "jump to settings" key — the stack already expresses it, and a jump
  would be a second spelling of one thing.

- **Scopes are read off the response, never tabulated.** Every GitHub reply
  carries `X-Oauth-Scopes` (what the token has) and `X-Accepted-Oauth-Scopes`
  (what the endpoint would accept, any one of them) — on FAILED replies too.
  So a denial names its own fix and there is no endpoint-to-scope table to
  drift. `gh api -i` on every call is what makes that free; the token's own
  scopes are learned from whatever request happened to run, so there is no
  startup probe and must not become one.

  **A 404 has to be translated.** `GET /user/keys` without the scope answers
  **404, not 403** — rendered literally that is "you have no SSH keys" on an
  account with two. `Got::Denied` is kept apart from `Got::Failed` for exactly
  this: one is "your token cannot see this, here is the line that changes
  that", the other is "something went wrong".

  **It suggests the NARROWEST accepted scope.** Offered
  `admin:public_key, read:public_key, write:public_key`, it says
  `read:public_key`. `gh`'s own message suggests `admin:` because it cannot
  know what the caller intends; this screen only reads. Writing keys asks for
  `write:` when it exists.

  Enter on the denied row RUNS the fix via `Flow::Detach` — `gh auth refresh`
  is interactive and needs the real tty, and it is self-confirming because gh
  says what it is about to do and waits. The screen then re-asks on the next
  tick, via a flag rather than before the detach: the detach is what changes
  the token, so a request fired beforehand races it and comes back denied,
  which looks exactly like the authorisation having failed.

- **Tokens and 2FA are said out loud, not shown as empty lists.** Neither has
  an API at any scope. A blank list reads as "none" and would be wrong, so the
  Account tab names them as unreachable instead. Absent information and absent
  data are different claims.

- **There is no Enterprise level, and that was measured rather than assumed.**
  Every org on this account is free or team plan and the token has no
  `read:enterprise`, so GraphQL refuses `viewer.enterprises` outright. A screen
  for something that cannot exist is maintenance for nothing. If one ever
  appears it is a level above `orgs` and the seeded stack already takes it.

- **`github` is installed, and the swap is done** (2026-10-24).
  `bash/bash_github_tui` and the `github()` function in it are deleted,
  `deps.conf`'s `[tui]` declares `tool manual github`, and `tui/deps.sh` has it
  in `crates=()`. For most of the port it deliberately did NOT install: a bash
  function beats PATH, so a binary installed alongside `github()` could not
  have won anyway, and wiring up a half-ported dispatcher would have thrown
  away the one thing that made this a deletion rather than a rewrite -- the CI
  rule that only `bash_github_tui` could invoke fzf.

  Three things the swap needed that the parity table did not cover, all found
  by grepping for every mention of the file rather than by the gate:
  `bashrc` sourced `~/.bash_github_tui`; `deps.conf` linked it; and
  `_gh_tab_var` wrote the `gh_tui` user var that wezterm reads to mark the tab
  (see `TabMark` below). A table derived from a dispatcher covers exactly what
  it derives from.

  **The precondition for the swap is a table, not a judgement.**
  [`docs/decisions/github-parity.md`](docs/decisions/github-parity.md) has a
  row per `_ghw_main` verb, DERIVED from the dispatcher so a verb cannot be
  silently absent, and CI blocks `github` entering `tui/deps.sh`'s `crates=()`
  while any row is `todo` or `partial`. Both directions are asserted. The
  statuses are a measurement except `dropped`, which is a decision and needs an
  argument in the row.

  **The `No bash function shadows a tui binary` CI row was RED for the
  duration of the port, deliberately, and it is green now.** It derives the
  binary names from `tui/Cargo.toml`'s `members`, so adding `tui/github/` to
  the workspace — which building and testing it required — started it covering
  `github` while `github()` still existed. Its own comment predicted this:
  *"fails until the bash function is gone, which is exactly the handover it
  should force."* It was never narrowed to the installed set to buy a green
  tick, and it should not be: the next binary added here gets the same
  deadline.

- **`TabMark` is the one piece of this that talks to wezterm.** `github` sets
  the `gh_tui` user var (OSC 1337) on startup and clears it on the way out, and
  `wezterm/wezterm.lua`'s `format-tab-title` reads it to put GitHub's Octicons
  mark on the tab. It is a `Drop` guard rather than two calls for the reason
  the bash version hung the clear on a `RETURN` trap: the variable is state
  inside somebody else's process, and one left set leaves the pane wearing the
  mark for the life of the shell. `Drop` runs on the way out of `main` and on
  an unwinding panic alike — `tui-core`'s panic hook restores the terminal and
  then calls the previous hook, so unwinding continues and the guard fires.

  It writes to `/dev/tty`, not stdout: the escape is addressed to the terminal
  and must not land in a pipe. The var name and the base64 values are the wire
  format `wezterm/wezterm.lua` reads, so a change here goes dark rather than
  failing — `lib.rs`'s test pins the exact bytes.

- **`index` groups by PATH and identifies by REMOTE, because on the real tree
  neither can do both.** Measured before the reader was written:
  `~/github/orgs/acme` holds the org `acme-co`; depth is not fixed
  (`orgs/acme/clients/initech/portal`); `private/repos/jarvis-project` holds
  other people's clones; one repo has no remote. Grouping by owner would file
  someone else's clone under its own heading and have nowhere to put the
  remoteless one. So the lane comes from where you filed it and the slug comes
  from `origin`.

  Both are read as FILES — `.git/config` and `.git/HEAD` parsed directly, not
  `git config` and `git branch`. Twenty-seven process spawns is the difference
  between a screen that opens instantly and one you watch open, and instant is
  this screen's entire claim. `current()` walks up for `.git` rather than
  asking `gh repo view`, for the same reason plus working offline.

- **A row says the unusual thing and stays quiet otherwise.** The first version
  put `owner/name` on every row, which on the real tree is thirteen rows of
  `roest1/<the-directory-name>` under a heading that already said `roest1` —
  and the column truncates, so the noise pushed the genuinely different ones
  (`bilawalsidhu/gods-eye-view`, `local only`) off the end. Now the slug shows
  only when the directory was renamed or the owner is not the lane's, and the
  branch badge only when it is not `main`/`master`. Same rule as `dots`' `off`
  badge.

- **The Actions screen runs TWO clocks, and both stop by themselves.** The list
  is re-asked every 15s, the selected run's jobs every 5s — each only while the
  thing it is watching is unfinished, and each re-armed from what ARRIVED
  rather than on a fixed timer. A list of completed runs schedules nothing, so
  there is no stop button to find and no timer left running behind a cursor
  that has moved on. The jobs clock follows the CURSOR, which is the half most
  easily got wrong: visiting five runs during a build must not leave five
  timers behind.

- **A toggle that cannot come back is not a toggle.** The Actions screen is
  reachable two ways — the repo's 🚀 tab, and the pull request menu's "Workflow
  runs for this commit", which is bash's `ctrl-a` as a row — and `b` offers a
  DIFFERENT pair in each. Opened on a repo it is every branch or the one checked
  out here. Opened on a commit it is that commit or the branch carrying it,
  because widening a commit scope to every branch is a door that does not come
  back: nothing on screen afterwards names the commit again, so the only route
  to it is `q` out and open the menu a second time.

  So `Actions` remembers the commit it was OPENED for, separately from the
  scope `b` has got to since. `scope` is where you are; `commit` is what the
  screen is about, and the second is what makes the first reversible.

  `for_commit` takes a slug and a sha rather than an `index::Repo`, which is
  the whole reason a second constructor had to exist: a pull request's head
  commit need not be anywhere on this disk — a fork, or a branch this clone has
  never fetched — so there is no `git rev-parse` to do and nothing local to
  read. It is handed the pull request screen's own fetcher, which is the same
  repo, so a completed run's jobs and logs come from what the Actions tab
  already has.

  **The HEAD marker switches off inside a commit scope**, because there every
  run IS that commit and a dot on every row says nothing. Widen to the branch
  and it comes back, now answering the question you actually have: which of
  this branch's runs belong to the commit I came from.

  **Inside a commit the row is the WORKFLOW and its run number, not the run
  title.** Every run of one commit carries the same `display_title` — the pull
  request's — so a list of titles is a column of identical rows; the first
  real repo this was pointed at drew five. The workflow is what identifies a
  row there, with the number because a workflow can run several times on one
  commit (three `pull_request_target` runs of the same workflow, measured), and
  it goes in the LABEL rather than the detail because the detail is the first
  thing a 52-column list truncates away. Spelt as the pane's header spells it.

- **A scope toggle has to RE-READ what the fetcher already has.** `Actions::tick`
  syncs when the fetcher's epoch moves, which is the right rule for an arrival
  and the wrong one for a toggle: switching back to a scope answered a moment
  ago makes nothing arrive, so the screen sat on an empty list saying "no
  workflow runs here yet" about a scope with six runs in it. `toggle_scope`
  calls `sync()` itself now.

  Worth keeping because of how it hid. A live run re-arms the list clock and
  that refresh DOES bump the epoch, so on any repo with something building the
  list reappeared a second later and the bug was invisible. It only showed on a
  scope where everything had finished — which is every commit scope on a pull
  request that has stopped moving, i.e. most of them.

  `Fetcher::seed` exists for this, `#[cfg(test)]` only: `request` reaches the
  network and `get` is the only way back out, so how a screen behaves on an
  answer it ALREADY HAS was otherwise untestable. It deliberately does not bump
  the epoch — that is the condition being tested, not a quirk of the seam.

- **Only what cannot change is cached across sessions.** A completed run's jobs
  and a completed job's log are immutable, so they live in
  `~/.cache/dotfiles-github/<owner>/<name>` and last week's failure opens
  instantly in a shell that has never seen it. A run LIST is a question about
  now and a live run's jobs are the thing being watched change, so neither
  touches the disk.

  **The cache key carries the ATTEMPT**, not just the run id. A rerun is the
  same id with entirely new jobs, so a key on id alone serves the previous
  attempt forever — which is why the bash implementation had to delete cache
  files on rerun. Putting the attempt in the key retired the deletion.

  **Whether an answer may be cached is the caller's claim, from the list it is
  already showing** (`Req::Jobs { persist }`), which saves the second `gh api`
  call the bash version made to ask a run its own status. That staleness is
  safe in only one direction and it is the one that matters: a 15-second-old
  list can say "running" about a run that just finished, and we merely decline
  to cache. It can never say "completed" about a run still going, because
  completed is terminal.

- **The step-log slicing is ported line for line, and its tests came first.**
  GitHub serves no per-step logs — the archive is one file per job, which is
  why `gh run view --log` prints `UNKNOWN STEP` — so a step's output is
  reconstructed: a timestamp window, then the step's own `##[group]Run` line as
  an anchor, then a cut at the last `##[error]`. The anchor PREFERS a group
  whose text is the step's name, which is what makes unnamed `uses:` steps work
  and what stops a window that opened a second early from returning the
  previous step's output.

  Tests before implementation here alone, and the reason is the failure mode
  rather than the difficulty: a slice off by one boundary returns plausible log
  output for the wrong step, and nothing about the result looks wrong. It
  compares timestamps as STRINGS, as the awk original does — fixed-width ISO
  8601 sorts lexicographically, so parsing them into instants would be slower
  and no more correct.

- **Status words, not emoji, in a `Row::badge`.** The badge is right-aligned
  into a fixed grid and an emoji is double-width in some terminals and single
  in others, so a column of them lines up nowhere reliably. `ok`/`fail`/`run`
  with a colour index does. The PANE is free prose and keeps the glyphs.

- **One `Verdict`, derived in one place.** `status` and `conclusion` together
  have more states than anyone needs to reason about, and reasoning about them
  per call site is what let the bash renderers drift on which field decided a
  job's colour. Both unknown-state directions fail safe deliberately: a
  conclusion GitHub adds later reads as `Other` rather than as a failure, and a
  STATUS it adds later reads as running rather than as finished — because a run
  wrongly called finished stops being polled, which is the error that does not
  correct itself.

- **`github`'s confirm is screen-local and safe by construction, not by
  bookkeeping.** `e` and `x` cost runner minutes or kill a build, so they ask;
  the only key that acts is `y`, and any other key the screen sees — plus any
  cursor movement, via `focus` — withdraws. That is deliberately weaker than a
  real dialog: a shared confirm WITH text entry is still the right home, since
  `pr-create`, `secret-set` and every delete need one, and that is a `tui-core`
  change rather than five screen-local copies of this.
