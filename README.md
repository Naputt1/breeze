# Breeze

Breeze keeps **local-only file overlays** for Git worktrees. Machine-specific
files and small working-tree patches stay out of commits while remaining
consistent across every worktree of a repository. Breeze also deep-merges
overlay configuration into `fog` configuration without touching tracked files.

- Links are symlinks from a worktree path to a file in Breeze's private store.
- Patches are three-way diffs recorded against a tracked file, re-applied on
  demand.
- Nothing is written to Git history, remotes, or tracked files unless you ask
  with `breeze promote`.

## Requirements

- A Rust toolchain with Cargo (edition 2021, Rust 1.70 or newer).
- macOS or Linux. Symlinks and Unix permissions are used.
- Optional: `fog` on `PATH` for `breeze fog` and overlay merging.

## Build and install

```sh
cargo build --release
cargo test
```

Install the binary on your `PATH`:

```sh
cargo install --path .
# or
install -m 0755 target/release/breeze ~/.local/bin/breeze
```

Run from a checkout without installing with `cargo run -- <command>`.

## Quickstart

```sh
cd ~/code/my-app
breeze init                      # create the store for this repository
breeze link config/local.toml    # untracked file, shared by every worktree
breeze env --write               # write merged overlay env values to .env.local
breeze env --write --force       # back up an existing .env.local, then replace it
breeze fog -- dev                # run fog with merged overlay configuration
```

Inside a second worktree, `config/local.toml` is already a symlink to the same
store file, so editing it there changes it everywhere.

For a tracked file that needs local changes:

```sh
breeze edit fog.json             # record a local patch
breeze sync --all                # apply it to every worktree
breeze diff fog.json             # inspect overlay changes
breeze revert fog.json           # drop the overlay and restore tracked content
```

## Commands

Global flags accepted by every command:

| Flag | Description |
| --- | --- |
| `--json` | Emit machine-readable JSON. |
| `--quiet` | Suppress informational output. |
| `--color <auto\|always\|never>` | When to color diagnostics (default `auto`, `NO_COLOR` respected). |
| `--repo <PATH>` | Operate on the repository containing `PATH` (default: current directory). |

| Command | Description |
| --- | --- |
| `breeze init` | Create the Breeze store for the repository. Safe to rerun. |
| `breeze status [--json]` | Show repository, worktree, store and overlay status. `--json` includes per-override `path`/`mode`/`layer`/`source`, stale patches, and a doctor-lite `issues` count. |
| `breeze link <PATH> [--layer shared\|branch\|worktree] [--all-worktrees]` | Link an untracked file. The default layer is `shared`; `branch` and `worktree` override it locally. `--all-worktrees` is accepted for compatibility; a link already applies to every worktree its layer covers. |
| `breeze unlink <PATH> [--force]` | Remove link overrides and restore real files. `--force` allows replacing locally modified copies, with a backup. |
| `breeze edit <PATH> [--layer shared\|branch\|worktree] [--editor <CMD>]` | Edit a tracked file and record the result as a patch (default layer `branch`). |
| `breeze sync [--all] [--path <PATH>] [--dry-run]` | Repair symlinks and re-apply patches. Defaults to the current worktree; `--all` covers every worktree. |
| `breeze diff [PATH] [--stat]` | Show changes between overlays and the working tree. |
| `breeze revert <PATH> [--force]` | Remove overlay changes for a file and restore tracked content. |
| `breeze promote <PATH>` | Turn a recorded patch into ordinary tracked content, ready to commit. |
| `breeze share <PATH>` | Move a branch or worktree override up to the shared layer. |
| `breeze env [--format dotenv\|shell\|json] [--write] [--show] [--reveal]` | Print the resolved environment. Values are masked unless `--reveal` is confirmed on a terminal. `--write` writes the merged values to `.env.local` in the current worktree; an existing file is backed up in the store and replaced only with `--force`. |
| `breeze run -- <CMD> [ARGS...]` | Run a command with overlay environment variables applied. |
| `breeze fog [ARGS...]` | Forward arguments to `fog`, merging overlay configuration first. |
| `breeze guard install [--with-post-checkout]` | Install the `pre-commit`/`pre-push` guard hooks. `--with-post-checkout` additionally installs an opt-in hook that runs `breeze sync --all` after checkout and worktree creation. |
| `breeze guard uninstall` | Remove managed hooks and restore backed-up hooks. |
| `breeze guard status` | Report installed hooks and backups. |
| `breeze doctor [--fix]` | Check links, patches, stale bases, duplicates, permissions and missing worktrees. `--fix` repairs what it safely can. |
| `breeze gc [--force]` | Remove store data for worktrees that no longer exist, stale fog temp configs, empty directories and orphan patches. `--force` also deletes unused store files and keeps only the five newest backups per repository. |
| `breeze version` | Print the version. |
| `breeze completions <bash\|zsh\|fish>` | Print a completion script (hidden helper). |

### Layers

Overrides resolve from least to most specific: `shared`, then `branch`, then
`worktree`. A more specific layer shadows a broader one. The current layer is
chosen automatically for `sync`, and recorded explicitly for `link`/`edit`.
`breeze share <PATH>` moves a branch or worktree link or patch to `shared`;
a shared patch applies to every branch and worktree on the next
`breeze sync --all`.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success. |
| `1` | General failure. |
| `2` | Usage error: bad arguments, missing override, or nothing to do. |
| `3` | Patch conflict: a patch did not apply cleanly. |
| `4` | Repository not initialized; run `breeze init`. |
| `5` | Unsafe operation refused (tracked file, differing copy, custom hooks path, …). |
| `6` | A guard hook blocked a commit or push. |
| `126` | `breeze run` could not execute the command (permission denied). |
| `127` | `breeze run` could not find the command. |
| other | `breeze run` exits with the child process's status. |

## Store layout

The store lives outside the repository, under the platform data directory
(`$XDG_DATA_HOME/breeze/repos/<repo_id>`, usually
`~/.local/share/breeze/repos/<repo_id>`). Directories are mode `0700` and
files `0600`.

```
repos/<repo_id>/
  repo.json                 repository metadata and schema version
  shared/
    files/<path>            link targets in the shared layer
    patches/<path>.patch    shared patches
    patches/<path>.json     patch metadata (base blob, applied hash)
    env                     overlay environment (input for `breeze env`/`run`)
    fog.local.json          overlay fog configuration
  branches/<branch_id>/
    files/ patches/ env? fog.local.json?
  worktrees/<worktree_id>/
    files/ patches/ fog.local.json?
  state/
    overrides.json          recorded overrides
    events.log              append-only action log
    lock
  backups/                  copies made before destructive operations
```

An `exclude` block in `.git/info/exclude` keeps overlay paths out of
`git status` without modifying tracked `.gitignore` files.

## Safety notes

- Breeze never uploads or prints file contents; `env` masks values unless you
  explicitly reveal them on a terminal, and never logs environment values.
- Operations that would overwrite something differing are refused with exit
  `5`. `env --write --force`, `unlink --force` and `revert --force` make backups
  in the store first.
- Store files are never read or written through symlinks, so a store copy
  swapped for a link outside the store is reported as broken instead of
  followed.
- The guard hooks block commits and pushes that include overridden paths,
  unless `BREEZE_ALLOW_COMMIT=1` is set. They chain to pre-existing hooks.
- The post-checkout hook is opt-in, runs `breeze sync --all` best-effort, and
  always exits `0`; it never fails a checkout.
- `gc` never touches the current branch layer or files backing live overrides
  unless `--force` is given.
- `promote` is the only command that turns local overlay content into tracked
  content; it still leaves the actual `git add`/`git commit` to you.

## Testing

```sh
cargo fmt --check
cargo build
cargo test
```

Integration tests create temporary Git repositories and worktrees, set their
own `XDG_DATA_HOME` and `XDG_CACHE_HOME`, and exercise the built binary.

## Shell completions

```sh
breeze completions bash > ~/.local/share/bash-completion/completions/breeze
breeze completions zsh  > ~/.zsh/completions/_breeze
breeze completions fish > ~/.config/fish/completions/breeze.fish
```
