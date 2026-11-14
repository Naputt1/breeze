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
| `breeze sync [--all] [--path <PATH>] [--dry-run] [--links-only]` | Repair symlinks and re-apply patches. `--links-only` repairs links without applying tracked patches. Defaults to the current worktree; `--all` covers every worktree. |
| `breeze diff [PATH] [--stat]` | Show changes between overlays and the working tree. |
| `breeze revert [<PATH>] [--all] [--force]` | Remove overlay changes for one file or every patch and restore tracked content. |
| `breeze promote <PATH>` | Turn a recorded patch into ordinary tracked content, ready to commit. |
| `breeze share <PATH>` | Move a branch or worktree override up to the shared layer. |
| `breeze env [--format dotenv\|shell\|json] [--write] [--show] [--reveal]` | Print the resolved environment. Values are masked unless `--reveal` is confirmed on a terminal. `--write` writes the merged values to `.env.local` in the current worktree; an existing file is backed up in the store and replaced only with `--force`. |
| `breeze run -- <CMD> [ARGS...]` | Run a command with overlay environment variables applied. |
| `breeze fog [ARGS...]` | Forward arguments to `fog`, merging overlay configuration first. |
| `breeze guard install [--with-post-checkout] [--with-pull-hooks] [--with-all-hooks]` | Install the `pre-commit`/`pre-push` guard hooks. `--with-post-checkout` adds `post-checkout`; `--with-pull-hooks` adds `post-merge`/`post-rewrite`; `--with-all-hooks` adds every available hook (`post-checkout`, `post-merge`, `post-rewrite`, `post-applypatch`, and a best-effort `pre-rebase`). |
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

### Guard hooks

`breeze guard install` manages `pre-commit` and `pre-push` hooks that block
commits and pushes containing local-only paths. The other families are
opt-in; `--with-all-hooks` installs every hook Breeze knows how to use.

| Flag | Hooks | Purpose |
| --- | --- | --- |
| none | `pre-commit`, `pre-push` | Block local-only paths from being committed or pushed. |
| `--with-post-checkout` | `post-checkout` | Repair links after a branch/worktree checkout; tracked patches are not applied. |
| `--with-pull-hooks` | `post-merge`, `post-rewrite` | Sync after a merge, pull, rebase or amend. |
| `--with-all-hooks` | all of the above plus `post-applypatch`, `pre-rebase` | Adds the remaining opt-in hooks; `pre-rebase` is best effort. |

The sync and guard hooks embed the absolute path of the `breeze` binary that
installed them and the main worktree root, shell-quoted, and never look up
`breeze` on `PATH`; `pre-rebase` uses the worktree Git is rebasing. A GUI or IDE Git launch with a minimal environment therefore
runs the same binary, and a different `breeze` earlier on `PATH` is never
executed. Because hooks live in the shared Git directory, installing from a
linked worktree still embeds the main worktree, so removing the linked worktree
does not break the hooks.

If the embedded binary has been moved or deleted, the hook prints one warning
and exits `0`; reinstall after moving or replacing the binary. If `breeze
status` fails for a reason other than an uninitialized repository (exit `4`) —
for example because the embedded root no longer exists — the hook prints a
warning naming the stale root and exits `0`. Reinstall from the current
checkout after moving a repository.

The opt-in hooks are best effort. They first run `breeze status` with the
embedded binary and exit `0` silently when the repository has never been
initialized. They suppress sync output and always exit `0`; when a sync cannot
complete they print a warning to stderr so the Git operation still completes:
the links-only `post-checkout` hook prints `breeze: overlay sync needs
attention; run breeze doctor`, and the full-sync hooks print `breeze: patch
sync needs attention; run breeze doctor`.

- `post-checkout` repairs links only on branch or worktree checkouts (Git's
  third argument is `1`). It never applies tracked patches: Git also invokes
  `post-checkout` for its own internal checkouts during rebase, cherry-pick and
  revert, and applying a patch there can dirty the worktree and abort the
  operation. A normal branch switch therefore leaves patch records active but
  does not reapply them; run `breeze sync` when you want patches back. File
  checkouts (`git checkout -- <file>`, `git restore`) pass `0` and do nothing.
- `post-merge`, `post-rewrite` and `post-applypatch` run a full
  `breeze sync --all`.
  `post-rewrite` serves both rebases and `git commit --amend`; `post-applypatch`
  runs after a successful `git am`.
- `pre-rebase` lists active tracked patches and blocks with exit `6`, pointing
  at `breeze revert --all`, and allows the rebase when no patches are active.
  This is best effort: Git rejects a dirty worktree before running any
  `pre-rebase` hook, so a normal applied patch aborts with Git's own
  "unstaged changes" error before Breeze can speak. Neither the patch content
  nor its record is changed in that case. There is no auto-revert or suspend
  mode: `breeze revert` deletes the record, and running it from `pre-rebase`
  would leave the worktree dirty and make Git abort the rebase anyway. Run
  `breeze revert --all` explicitly before rebasing.
- `pre-merge-commit` and `pre-applypatch` are deliberately not installed:
  they run before the index/worktree is complete, so they cannot decide safely
  whether to sync, and `post-merge`/`post-applypatch` cover the success case.
- `post-commit` and `post-index-change` are deliberately not installed: they
  fire for every commit and every index write (including Breeze's own work),
  cannot restore lost content, and would add noise without preventing damage.

Existing hooks with the same names are backed up as `<hook>.breeze-backup` and
chained with their original arguments and stdin. Reinstalling is idempotent and
never overwrites a backup. Breeze refuses to install when `core.hooksPath`
points somewhere else. Hooks live in the shared Git directory, so one install
covers every worktree; installing from a linked worktree is supported and
embeds the main worktree root. New clones need their own install.

### Git operation matrix

`sync` means a full `breeze sync --all`; `post-checkout` runs
`breeze sync --all --links-only` and never applies tracked patches. "none"
means there is no safe Git hook for that operation and Breeze does not claim
coverage.

| Operation | Hook | Behavior | Still at risk |
| --- | --- | --- | --- |
| `commit` | `pre-commit` | Blocks staged local-only paths (exit `6`). | `BREEZE_ALLOW_COMMIT=1` / `--no-verify` bypasses it. |
| `commit --amend` | `pre-commit`, `post-rewrite` | Blocks overrides, then syncs after rewrite. | Amending changes the patch base; refresh with `breeze edit`. |
| `push` | `pre-push` | Blocks pushed commits containing overrides. | `--no-verify`; server-side protection is authoritative. |
| `merge` (FF and non-FF) | `post-merge` | Syncs after success. | Aborted/conflicted merges run no post hook; dirty patches can abort first. |
| `pull` (merge, ff-only) | `post-merge` | Syncs after success. | Failed pulls run no post hook. |
| `pull --rebase` | `post-checkout` (links only), `post-rewrite`; `pre-rebase` if installed | Repairs links at the rebase checkout, then full sync after a successful rebase. | Conflicts/aborts run no post hook; dirty patches usually stop Git before `pre-rebase`. |
| `rebase` success | `post-checkout` (links only), `post-rewrite` | Repairs links, then full sync after the rewrite. | Patch may conflict; resolve, then `breeze edit` to refresh its base. |
| `rebase` conflict/abort | `post-checkout` (links only); `pre-rebase` before; none after | `pre-rebase` can block only if Git reaches it; the rebase checkout repairs links only. | No sync after abort; run `breeze sync --all`. |
| `checkout`/`switch` | `post-checkout` (arg `1`) | Repairs links only; tracked patches are not applied. | Run `breeze sync` to apply patches; dirty patches can still make checkout abort. |
| `checkout -- <file>` | `post-checkout` (arg `0`) | Intentionally does nothing. | Patch record stays active while the file is back at HEAD; use `breeze revert <file>`. |
| `restore` | `post-checkout` (arg `0`) | Intentionally does nothing. | Same as above. |
| `reset --hard` | none | — | Discards patched content; the patch record remains; use `breeze revert --force`. |
| `reset --mixed` | none | — | Worktree keeps patch content; index resets; next sync/commit needs care. |
| `stash`/`stash pop` | none | — | Stash may capture patch content; pop conflicts are yours to resolve, then sync/edit. |
| `clean -fdx` / `-fdX` | none | — | Deletes ignored link symlinks; run `breeze sync --all`. Patched tracked files are not removed. |
| `cherry-pick` | `pre-commit`, `post-checkout` (links only) | Git runs `pre-commit` for each picked commit and repairs links at its checkout; tracked patches are not applied. | Breeze installs no post-commit hook; resolve conflicts and run `breeze sync --all` manually. |
| `revert` (Git) | `pre-commit`, `post-checkout` (links only) | Git runs `pre-commit` for the new revert commit and repairs links at its checkout; tracked patches are not applied. | No Breeze post-commit sync; run `breeze sync --all`, then `breeze edit` if a patch base drifted. |
| `apply` (plain) | none | — | No hook at all; run `breeze sync --all` manually. |
| `am` | `pre-commit`, `post-applypatch` | Git runs `pre-commit` per patch and Breeze syncs after each successful patch. | A failed series ends before `post-applypatch`; sync manually. |
| `worktree add` | `post-checkout` (arg `1`) | Repairs links in the new worktree; tracked patches are not applied. | Untracked files not managed by Breeze are not copied; run `breeze sync --all` for patches. |
| `worktree remove/move` | none | — | Store entries go stale; `breeze gc` prunes removed worktrees. |
| `filter-repo`/`filter-branch` | none | — | Rewrites patch bases; revert patches before filtering, then re-edit. |
| `submodule update` | none | — | Submodules are not managed; `link` refuses them. |
| `commit`/`push --no-verify` | none | Bypasses `pre-commit`/`pre-push`. | Post hooks still run; do not use it to force local overrides. |
| GUI/IDE with minimal `PATH` | all installed hooks | Embedded absolute binary still runs. | If the binary moved, the hook warns and no-ops; reinstall. |

### Residual risks and recovery

- Aborted pulls and rebases do not run `post-merge`/`post-rewrite`. Run
  `breeze sync --all` after resolving or aborting.
- Dirty patched files can make `git pull`, `merge`, `checkout` or `rebase`
  abort before any hook runs. Breeze never hides patch content to avoid this.
- Branch and worktree checkouts repair links only; tracked patches are not
  reapplied automatically. Run `breeze sync` (or `breeze sync --all`) after
  switching branches when you need the patches back.
- `git clean -fdx` and `git clean -fdX` delete ignored link symlinks. Run
  `breeze sync --all` to recreate them from the store.
- `git reset --hard`, `git restore` and `git checkout -- <file>` can discard
  tracked patch content. The patch record is not automatically dropped; use
  `breeze revert <file>` to remove it, or `breeze sync --all` to reapply it.
- `--no-verify` bypasses the commit and push guards, and a custom
  `core.hooksPath` is refused rather than fought. Server-side push protection
  remains the authoritative safeguard.
- The embedded main-worktree root and binary path become stale if the
  repository or Breeze binary moves; the next hook warns and no-ops, so rerun
  `breeze guard install` from the current checkout. A fresh clone has no hooks
  at all.
- When `breeze sync` reports a conflict, resolve the file, run `breeze edit`
  on it to refresh the patch base, and retry. `breeze doctor` reports stale
  patches whose base no longer matches `HEAD`.
- `pre-rebase` cannot protect a worktree Git already considers dirty, which is
  the normal state of an applied patch. `post-checkout` never applies tracked
  patches, by design, because Git invokes it during internal rebase,
  cherry-pick and revert checkouts as well as explicit branch switches.
- There is no safe automatic patch suspension for rebases. `breeze revert`
  removes records rather than suspending them, so automatic reverting inside
  `pre-rebase` is not offered; use `breeze revert --all` explicitly, rebase,
  then re-apply with `breeze edit`/`breeze sync`.
- There is no `breeze git`/`breeze pull` wrapper in this version; the manual
  recovery for any uncovered operation is `breeze sync --all`, plus
  `breeze revert --all` before history rewrites and `breeze edit` after
  resolving a patch conflict.

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
- The `post-checkout`, `post-merge`, `post-rewrite` and `post-applypatch`
  hooks are opt-in. `post-checkout` runs `breeze sync --all --links-only` and
  repairs links without applying tracked patches, because Git invokes it for
  internal rebase, cherry-pick and revert checkouts. The other three run a
  full `breeze sync --all`. All are best effort and always exit `0`; a failing
  or conflicting sync only prints a warning, so checkouts, pulls, rebases,
  amends and `git am` never fail because of Breeze. They embed the absolute
  `breeze` path and never execute a binary found on `PATH`.
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
