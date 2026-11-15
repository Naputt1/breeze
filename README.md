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
breeze edit fog.json --name api-timeout  # record a named local patch
breeze sync --all                # apply it to every worktree
breeze diff fog.json             # inspect overlay changes
breeze revert fog.json           # archive the patch, restore tracked content
breeze restore api-timeout       # re-apply an archived patch later
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
| `breeze status [--json]` | Show repository, worktree, store and overlay status. `--json` includes per-override `path`/`mode`/`layer`/`source`/`name`, stale patches, and a doctor-lite `issues` count. Named patches are shown as `name (path)`. |
| `breeze link <PATH> [--layer shared\|branch\|worktree] [--all-worktrees]` | Link an untracked file. The default layer is `shared`; `branch` and `worktree` override it locally. `--all-worktrees` is accepted for compatibility; a link already applies to every worktree its layer covers. |
| `breeze unlink <PATH> [--force]` | Remove link overrides and restore real files. `--force` allows replacing locally modified copies, with a backup. |
| `breeze edit <PATH> [--layer shared\|branch\|worktree] [--editor <CMD>] [--name <NAME>]` | Edit a tracked file and record the result as a patch (default layer `branch`; filter mode uses only `shared`). `--name` sets or updates a repository-unique patch name (`[a-z0-9][a-z0-9._-]{0,63}`), stored in both the override and patch metadata. |
| `breeze sync [--all] [--path <PATH>] [--dry-run] [--links-only]` | Repair symlinks and re-apply patches. In filter mode it never writes patches into the worktree; it dry-runs each smudge merge and reports deferred patches. `--links-only` repairs links without applying tracked patches. Defaults to the current worktree; `--all` covers every worktree. |
| `breeze diff [PATH] [--stat]` | Show changes between overlays and the working tree. Named patches use a `# patch: <name> (<layer>, <path>)` header. |
| `breeze revert [<PATH>] [--name <NAME>] [--all] [--force]` | Archive one file or every patch to `<layer>/backups/patches/`, then remove the overlay and restore tracked content. |
| `breeze restore <NAME> [--layer <LAYER>]` | Re-apply an archived patch by name and record it again. Use `--layer` when the name is archived in more than one layer. |
| `breeze patches [--json]` | List active and archived patches with name, path, layer, staleness and archive state. Read-only. |
| `breeze promote <PATH>` | Turn a recorded patch into ordinary tracked content, ready to commit. The patch is archived first. |
| `breeze share <PATH>` | Move a branch or worktree override up to the shared layer. The matching archive moves too. |
| `breeze env [--format dotenv\|shell\|json] [--write] [--show] [--reveal]` | Print the resolved environment. Values are masked unless `--reveal` is confirmed on a terminal. `--write` writes the merged values to `.env.local` in the current worktree; an existing file is backed up in the store and replaced only with `--force`. |
| `breeze run -- <CMD> [ARGS...]` | Run a command with overlay environment variables applied. |
| `breeze fog [ARGS...]` | Forward arguments to `fog`, merging overlay configuration first. |
| `breeze guard install [--with-post-checkout] [--with-pull-hooks] [--with-all-hooks]` | Install the `pre-commit`/`pre-push` guard hooks. `--with-post-checkout` adds `post-checkout`; `--with-pull-hooks` adds `post-merge`/`post-rewrite`; `--with-all-hooks` adds every available hook (`post-checkout`, `post-merge`, `post-rewrite`, `post-applypatch`, and a best-effort `pre-rebase`). |
| `breeze guard uninstall` | Remove managed hooks, restore backed-up hooks, and disable filter mode if it is enabled. |
| `breeze guard status` | Report installed hooks and backups, plus filter-mode health when enabled. |
| `breeze filter enable` | Opt this repository into filter mode: write the managed `.git/info/attributes` block and repo-local `filter.breeze` config, capture patched contents, and re-checkout registered paths. Refuses branch or worktree patch records (shared layer only). Safe to rerun. |
| `breeze filter disable` | Remove the attributes block and filter config and re-checkout registered paths to plain upstream content. Patch records are kept. |
| `breeze filter status` | Report whether filter mode is enabled, the configured binary and its presence, registered paths, and deferred patches. |
| `breeze doctor [--fix]` | Check links, patches, stale bases, duplicates, permissions, missing worktrees, and filter mode (deferred patches, captured content, binary, attributes/config). `--fix` repairs what it safely can. Named patches are identified as `name (path)`. |
| `breeze gc [--force]` | Remove store data for worktrees that no longer exist, stale fog temp configs, empty directories and orphan patches. Archived patches are reported but never deleted. `--force` also deletes unused store files and keeps only the five newest backups per repository. |
| `breeze version` | Print the version. |
| `breeze completions <bash\|zsh\|fish>` | Print a completion script (hidden helper). |

### Layers

Overrides resolve from least to most specific: `shared`, then `branch`, then
`worktree`. A more specific layer shadows a broader one. The current layer is
chosen automatically for `sync`, and recorded explicitly for `link`/`edit`.
`breeze share <PATH>` moves a branch or worktree link or patch to `shared`;
a shared patch applies to every branch and worktree on the next
`breeze sync --all`. Moving a patch also moves its archived copy.

Patch names are optional and repository-wide unique across live and archived
patches. `revert` archives a patch before deleting it: named patches keep their
name, while unnamed patches use a fallback of the sanitized path plus eight
hash characters (for example `fog-json-3f9c1a2b`). `breeze restore <name>`
re-applies an archive, records it again with its original name, and keeps the
archive. If the same name exists in several layers, pass `--layer`.

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

With filter mode enabled, `pre-commit`/`pre-push` stop blocking **patch** paths
because the clean filter already stages unpatched content; link paths remain
protected. `pre-rebase` also allows the rebase, since smudge re-merges patches
after the rewrite.

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
- `pre-rebase` lists active tracked patches as `name (path)` and blocks with
  exit `6`, pointing at `breeze revert --all` and `breeze restore` for later,
  and allows the rebase when no patches are active or filter mode is enabled.
  This is best effort: Git rejects a dirty worktree before running any
  `pre-rebase` hook, so a normal applied patch aborts with Git's own
  "unstaged changes" error before Breeze can speak. Neither the patch content
  nor its record is changed in that case. There is no auto-revert or suspend
  mode: reverting from `pre-rebase` would leave the worktree dirty and make Git
  abort the rebase anyway. Run `breeze revert --all` explicitly before
  rebasing, then `breeze restore <name>` afterwards.
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

### Filter mode (prototype)

Filter mode replaces the "apply the patch into the working tree and let hooks
re-apply it" model with Git's own clean/smudge filters. It is repo-wide and
opt-in:

```sh
breeze filter enable
breeze edit fog.json          # defaults to the shared layer in filter mode
breeze status                 # file shows the merged content; status is clean
```

`filter enable` writes a managed block to `.git/info/attributes`:

```
# >>> breeze managed >>>
fog.json filter=breeze
# <<< breeze managed <<<
```

and repo-local config (shared by every worktree):

```
[filter "breeze"]
    smudge = /absolute/path/to/breeze filter smudge %f
    clean  = /absolute/path/to/breeze filter clean %f
    required = true
```

The hidden `breeze filter smudge <path>` / `clean <path>` subcommands are the
filter entry points. They read content on stdin, write to stdout, and **always
exit 0**; they never prompt. They are registered only for paths that have a
shared patch. `breeze filter enable` also converts existing shared patch
records by deriving the full patched content; re-running it is idempotent.

- **smudge** runs whenever Git writes the file (checkout, pull, rebase, reset,
  stash pop, worktree add). No patch → pass the upstream content through.
  Otherwise Breeze three-way merges the stored base, stored patched content and
  upstream content. A clean merge becomes the working-tree file. A conflict
  writes the upstream content unchanged and marks the patch **deferred**: the
  tree stays clean and the patch is simply absent until the conflict is
  resolved with `breeze edit`.
- **clean** runs whenever Git reads the file (`status`, `diff`, `add`, stash).
  No patch → pass through. Otherwise it reverses the stored patch out of the
  working-tree content. Success produces the upstream content (plus any genuine
  manual edit, which stays dirty); failure passes the content through unchanged.
  This pairing guarantees that a deferred patch is also clean-passed, so
  `git status` does not report the patch as a modification.

Lifecycle:

- `breeze edit <path>` captures the full patched content, writes the record,
  registers the attributes line, then force-checkouts the file so smudge runs.
- `breeze revert` / `revert --all` / `promote` remove the record and the
  attributes line, then materialize plain content. `promote` deliberately
  leaves the patched content on disk and removes the filter registration first,
  so the usual `git add`/`git commit` records the patched content, not the
  unpatched upstream blob.
- `breeze sync` never writes patches into the worktree in filter mode. It
  dry-runs the smudge merge for each patch against the current index and prints
  `DEFERRED ...` for conflicts (exit `3`). Links are still repaired.
- `breeze status` marks deferred patches as `- path [deferred]` in human output
  and adds them to the JSON `deferred` array. `breeze doctor` reports deferred
  patches, missing captured content, missing stale context, a missing filter
  binary, and attributes/config mismatches; `--fix` regenerates missing
  content, rewrites a moved binary path, and repairs the attributes block.
- `breeze filter disable` removes the attributes block and config, then
  re-checkouts registered paths to plain upstream content. Patch records stay in
  the store, so `breeze sync` can apply them in patch mode later.
  `breeze guard uninstall` does the same cleanup when filter mode is enabled.

Filter mode is deliberately narrow in this prototype:

- Only the **shared** layer is supported. `breeze edit --layer branch` and
  `--layer worktree` exit `5` with "not supported in filter mode yet", and
  `filter enable` refuses a store that already contains non-shared patch
  records. Link mode is unchanged.
- `required = true` is intentional. If the configured binary is moved or
  deleted, Git operations that touch a filtered file fail loudly instead of
  silently committing unpatched content. `breeze doctor` and
  `breeze guard status` name the missing path; `breeze doctor --fix` points the
  config at the running executable.
- Fresh clones do not inherit `.git/info/attributes` or repo-local config; run
  `breeze filter enable` again in each clone.
- With filter mode enabled, the guard hooks stop blocking patch paths (the
  staged blob is already unpatched) and `pre-rebase` allows rebases; link paths
  are still protected.

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

#### With filter mode enabled

These rows assume `breeze filter enable` and at least one shared patch. Filter
mode handles the file inside Git, so the old hook rows above do not apply to
patched paths.

| Operation | Behavior | Still at risk |
| --- | --- | --- |
| `commit` / `push` | `clean` strips the patch; the file is not staged and the committed blob is unpatched. Guard hooks stop blocking patch paths. | A promoted file must have its record and attributes removed first (`breeze promote` does this). |
| `pull` / `merge`, unrelated change | Patch survives on disk; file stays merged and clean. | A failed or conflicted merge still needs manual resolution. |
| `pull` / `merge`, same hunk | smudge defers: file becomes upstream content, status stays clean. `sync`/`doctor` report it. | Deferred is silent until `sync`/`doctor`; resolve and re-run `breeze edit`. |
| `rebase` success | smudge re-merges the patch after the rewrite; `pre-rebase` allows it. | A patch that no longer fits is deferred, not failed. |
| `rebase` conflict / abort | The rebase checkout may defer; `--abort` restores the branch and smudge reapplies the patch. | Conflicts are still Git's to resolve; no Breeze sync runs after abort. |
| `checkout -- <file>`, `restore`, `reset --hard` | Git rewrites the file through smudge, so the patch reappears and status stays clean. | A manual tweak that conflicts with the patch can make `clean` pass it through dirty; resolve with `breeze edit` or discard. |
| `stash` / `stash pop` | The patch is invisible to Git, so a plain stash is a no-op; a real manual tweak round-trips and the patch is reapplied on pop. | Stash conflicts follow Git's usual rules; re-run `breeze edit` after resolving. |
| `worktree add` | The shared attributes and config make the new worktree materialize patched content through smudge. | Branch/worktree patch layers are not supported in filter mode. |
| Missing filter binary | `required = true` makes Git operations touching filtered files fail loudly. `doctor` and `guard status` report the missing path. | Recovery needs the binary back (or `doctor --fix` to point at the running one); a fresh clone needs `filter enable` again. |

### Residual risks and recovery

These apply to patch mode unless a bullet says filter mode.

- Aborted pulls and rebases do not run `post-merge`/`post-rewrite`. Run
  `breeze sync --all` after resolving or aborting.
- Dirty patched files can make `git pull`, `merge`, `checkout` or `rebase`
  abort before any hook runs. Breeze never hides patch content to avoid this.
  In filter mode the file is clean instead, so these operations proceed; the
  trade-off is that a conflict becomes a deferred patch.
- Branch and worktree checkouts repair links only; tracked patches are not
  reapplied automatically. Run `breeze sync` (or `breeze sync --all`) after
  switching branches when you need the patches back.
- `git clean -fdx` and `git clean -fdX` delete ignored link symlinks. Run
  `breeze sync --all` to recreate them from the store.
- `git reset --hard`, `git restore` and `git checkout -- <file>` can discard
  tracked patch content. The patch record is not automatically dropped; use
  `breeze revert <file>` to archive and remove it, `breeze restore <name>` to
  reapply an archive, or `breeze sync --all` to reapply it.
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
- `pre-rebase` cannot protect a worktree Git already considers dirty, which is
  the normal state of an applied patch, and `post-checkout` cannot reapply
  patches on a file restore by design. `post-checkout` never applies tracked
  patches, by design, because Git invokes it during internal rebase,
  cherry-pick and revert checkouts as well as explicit branch switches.
- There is no automatic patch suspension inside Git hooks. `breeze revert`
  archives patches instead of discarding them, so the recovery is to run
  `breeze revert --all` before a rebase and `breeze restore <name>` afterwards;
  resolve any conflicting restore, then run `breeze edit <path> --name <name>`.
- There is no `breeze git`/`breeze pull` wrapper in this version; the manual
  recovery for any uncovered operation is `breeze sync --all`, plus
  `breeze revert --all` before history rewrites and `breeze edit` after
  resolving a patch conflict.
- Filter mode: `required = true` means a moved or deleted binary aborts Git
  operations that touch a filtered file, instead of silently committing
  unpatched content. `breeze doctor` and `breeze guard status` report it;
  reinstall or run `breeze doctor --fix`.
- Filter mode: deferred patches are silent until `breeze sync` or
  `breeze doctor` runs. `breeze status` shows `[deferred]` for the affected
  paths, but Git itself only sees a clean tree.
- Filter mode: only shared-layer patches are supported. Branch/worktree layers
  are refused with exit `5`; run `breeze filter disable` before mixing layers.
- Filter mode: a manual edit adjacent to a patch can make the reverse merge
  conflict. `clean` then passes the file through, so the whole file (including
  the patch content) shows as dirty. Resolve, then re-run `breeze edit` or
  discard the tweak.
- Filter mode: a fresh clone has neither the attributes block nor the
  repo-local config. Run `breeze filter enable` in each clone.
- Filter mode: `promote` leaves patched content on disk and removes the record
  and attributes first. Do not run `git commit -a` on that file before the
  removal completes.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success. |
| `1` | General failure. |
| `2` | Usage error: bad arguments, missing override, duplicate patch name, or nothing to do. |
| `3` | Patch conflict: a patch did not apply cleanly, or an archive cannot be restored because the target is missing from `HEAD`. |
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
    patches/<path>.json     patch metadata (name, base blob, applied hash)
    patches/<path>.content  full patched content (filter mode)
    backups/patches/<name>.patch  archived shared patch
    backups/patches/<name>.json   archive sidecar
    env                     overlay environment (input for `breeze env`/`run`)
    fog.local.json          overlay fog configuration
  branches/<branch_id>/
    files/ patches/ backups/patches/ env? fog.local.json?
  worktrees/<worktree_id>/
    files/ patches/ backups/patches/ fog.local.json?
  state/
    overrides.json          recorded overrides
    events.log              append-only action log
    lock
  backups/                  copies made before destructive operations
```

An `exclude` block in `.git/info/exclude` keeps overlay paths out of
`git status` without modifying tracked `.gitignore` files. In filter mode a
managed block in `.git/info/attributes` registers patched paths for the
`breeze` filter; it is created and removed by `breeze filter enable`/`disable`
and kept in step with the shared patch records by `edit`, `revert`, `promote`,
`doctor --fix` and `gc`.

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
  unless `--force` is given. Archived patches are only reported, never deleted,
  and live under `0700` directories with `0600` files like the rest of the store.
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
