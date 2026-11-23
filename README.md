# github-tui

`github`: GitHub for the machine you are on, as a terminal UI. Run it anywhere for every clone under `~/github`, grouped by where you filed them. Run it inside a repo and it opens on that one.

- **The repo list reads the disk, not the API**, so it opens instantly and works offline. Set `GITHUB_DIR` if your clones live elsewhere.
- **Enter on a repo opens five tabs:** branches, pull requests, workflow runs, secrets and environments. A failing run opens with the failing step's log in the pane, and a run still going refreshes until it stops.
- **`hjkl` everywhere**, `/` to filter any list, and `enter` on a row for its actions.

Built on [roest1/tui-core](https://github.com/roest1/tui-core). [roest1/dotfiles](https://github.com/roest1/dotfiles) pins it as a submodule at `tui/github` and installs it with `make install tui`.

## Install

Needs a Rust toolchain, `git`, and the [GitHub CLI](https://cli.github.com/) signed in (`gh auth login`). Everything past the repo list goes through `gh api`.

```bash
cargo install --locked --git https://github.com/roest1/github-tui
```

## Develop

`cargo test` runs the tests, and `CLAUDE.md` has what not to undo. To work on tui-core at the same time, build against your checkout of it:

```bash
cargo build --config "patch.'https://github.com/roest1/tui-core'.tui-core.path='$HOME/github/tui-core'"
```

That rewrites `Cargo.lock` to the unpinned path, so run `git checkout Cargo.lock` before committing. CI's `--locked` build fails on a lock left that way.

`docs/decisions/github-tui.md` is the port's history (from a 2,500-line fzf script), and `docs/decisions/github-parity.md` is the table that proved nothing was lost.

## License

MIT. See [LICENSE](LICENSE).
