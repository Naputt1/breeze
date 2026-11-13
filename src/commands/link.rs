//! `breeze link` — move an untracked file into a store layer and symlink it.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::cli::LinkArgs;
use crate::commands::Ctx;
use crate::exit::{self, CommandError};
use crate::store::now_rfc3339;
use crate::store::overlay::{self, Layer, Repo};
use crate::store::overrides::{OverrideEntry, OverrideMode};

pub fn run(ctx: &Ctx, args: &LinkArgs) -> Result<i32> {
    let repo = Repo::open(ctx)?;
    let layer = Layer::parse(args.layer.as_deref())?;
    // Layer validation (branch layer needs a branch) happens here.
    let _ = repo.layer_dir(layer)?;

    let (relative, worktree_path) = overlay::validate_link_source(&repo, &args.path)?;

    if overlay::is_tracked(&repo.info.root, &relative)? {
        return Err(CommandError::new(
            exit::UNSAFE_REFUSED,
            format!(
                "`{relative}` is tracked by git; `breeze link` only handles untracked files\nuse `breeze edit` for tracked files"
            ),
        )
        .into());
    }

    let store_file = repo.store_file(layer, &relative)?;
    let store_exists = Repo::is_regular_store_file(&store_file);
    if !store_exists {
        if let Ok(metadata) = fs::symlink_metadata(&store_file) {
            return Err(overlay::unsafe_refused(format!(
                "store path {} exists but is not a regular file (found {:?})",
                store_file.display(),
                metadata.file_type()
            )));
        }
    }
    let source_bytes = if store_exists {
        fs::read(&store_file).with_context(|| format!("failed to read {}", store_file.display()))?
    } else {
        fs::read(&worktree_path)
            .with_context(|| format!("failed to read {}", worktree_path.display()))?
    };

    let targets = repo.targets_for_layer(layer)?;

    // Refuse before mutating anything when a real file disagrees.
    for target in &targets {
        let path = target.path.join(&relative);
        preflight(&path, &source_bytes, &repo.layout.repo_dir)?;
    }

    if !store_exists {
        repo.write_store_file(&store_file, &source_bytes)?;
    }

    let mut linked = 0;
    let mut already = 0;
    for target in &targets {
        let path = target.path.join(&relative);
        overlay::ensure_link_parent(&path)?;

        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                if overlay::link_points_to(&path, &store_file) {
                    already += 1;
                } else {
                    fs::remove_file(&path)
                        .with_context(|| format!("failed to remove {}", path.display()))?;
                    overlay::create_relative_symlink(&path, &store_file)?;
                    linked += 1;
                }
            }
            Ok(_) => {
                // Preflight guaranteed the contents match the store copy.
                fs::remove_file(&path)
                    .with_context(|| format!("failed to remove {}", path.display()))?;
                overlay::create_relative_symlink(&path, &store_file)?;
                linked += 1;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                overlay::create_relative_symlink(&path, &store_file)?;
                linked += 1;
            }
            Err(err) => {
                return Err(err).with_context(|| format!("failed to inspect {}", path.display()));
            }
        }
    }

    crate::store::exclude::add_path(&repo.exclude_path(), &relative)?;

    let source = repo.source_for(layer, &relative)?;
    let now = now_rfc3339();
    let mut overrides = repo.overrides()?;
    overrides.upsert(OverrideEntry {
        path: relative.clone(),
        mode: OverrideMode::Link,
        layer: layer.name().to_string(),
        source: source.clone(),
        tracked: false,
        base_sha: None,
        created_at: now.clone(),
        updated_at: now,
    });
    repo.save_overrides(&overrides)?;

    repo.log_event(if store_exists { "relink" } else { "link" }, &relative)?;

    if !ctx.quiet {
        if store_exists && linked == 0 {
            println!("already linked {relative} ({})", layer.name());
        } else {
            println!("linked {relative} ({})", layer.name());
        }
        println!("store:  {}", store_file.display());
        for target in &targets {
            let suffix = if target.current { " (current)" } else { "" };
            println!("link:   {}{suffix}", target.path.join(&relative).display());
        }
        println!("links:  {linked} created, {already} already correct");
    }

    Ok(exit::SUCCESS)
}

/// Refuse when a link would replace a real file with different contents.
fn preflight(path: &Path, source_bytes: &[u8], repo_dir: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to inspect {}", path.display())),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let target = fs::read_link(path)
                .with_context(|| format!("failed to read symlink {}", path.display()))?;
            let resolved = if target.is_absolute() {
                target
            } else {
                path.parent().unwrap_or(Path::new(".")).join(target)
            };
            let resolved = resolved.canonicalize().unwrap_or(resolved);
            let store_root = repo_dir
                .canonicalize()
                .unwrap_or_else(|_| repo_dir.to_path_buf());
            if resolved.starts_with(&store_root) || !resolved.exists() {
                Ok(())
            } else {
                Err(overlay::unsafe_refused(format!(
                    "`{}` is a symlink outside the breeze store; remove it before linking",
                    path.display()
                )))
            }
        }
        Ok(metadata) if metadata.is_file() => {
            let contents =
                fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
            if contents == source_bytes {
                Ok(())
            } else {
                Err(overlay::unsafe_refused(format!(
                    "`{}` differs from the store copy; compare and reconcile it manually, then rerun `breeze doctor`",
                    path.display()
                )))
            }
        }
        Ok(_) => Err(overlay::unsafe_refused(format!(
            "`{}` is not a regular file",
            path.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use crate::store::overlay::testutil::TempRepo;

    #[test]
    fn link_moves_file_and_creates_links_in_every_worktree() {
        let repo = TempRepo::new("link-shared");
        repo.write("config/local.toml", "from main\n");

        repo.breeze_ok(&["link", "config/local.toml"]);

        let store_file = repo.store_dir().join("shared/files/config/local.toml");
        assert_eq!(std::fs::read_to_string(&store_file).unwrap(), "from main\n");

        for root in [&repo.main, &repo.other] {
            let path = root.join("config/local.toml");
            let metadata = std::fs::symlink_metadata(&path).expect("link metadata");
            assert!(
                metadata.file_type().is_symlink(),
                "{} is not a link",
                path.display()
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "from main\n");
            assert!(repo.status_clean(root), "{} is dirty", root.display());
        }

        let exclude = std::fs::read_to_string(repo.main.join(".git/info/exclude")).unwrap();
        assert!(exclude.contains("# >>> breeze managed >>>"));
        assert!(exclude.contains("config/local.toml"));
        assert!(exclude.contains("# <<< breeze managed <<<"));

        let overrides =
            std::fs::read_to_string(repo.store_dir().join("state/overrides.json")).unwrap();
        assert!(overrides.contains("\"path\": \"config/local.toml\""));
        assert!(overrides.contains("\"mode\": \"link\""));
        assert!(overrides.contains("\"layer\": \"shared\""));
        assert!(overrides.contains("\"source\": \"shared/files/config/local.toml\""));
        assert!(overrides.contains("\"tracked\": false"));
        assert!(overrides.contains("\"updated_at\""));

        if cfg!(unix) {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&store_file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn edits_through_a_link_are_visible_in_other_worktrees() {
        let repo = TempRepo::new("link-through");
        repo.write("shared.txt", "one\n");
        repo.breeze_ok(&["link", "shared.txt"]);

        std::fs::write(repo.main.join("shared.txt"), "two\n").unwrap();
        assert_eq!(repo.read(&repo.other, "shared.txt"), "two\n");
    }

    #[test]
    fn link_is_idempotent_and_refuses_conflicts() {
        let repo = TempRepo::new("link-conflict");
        repo.write("data.txt", "same\n");
        repo.breeze_ok(&["link", "data.txt"]);
        repo.breeze_ok(&["link", "data.txt"]);

        let store_file = repo.store_dir().join("shared/files/data.txt");
        std::fs::write(&store_file, "store\n").unwrap();
        std::fs::remove_file(repo.other.join("data.txt")).unwrap();
        std::fs::write(repo.other.join("data.txt"), "different\n").unwrap();

        let (code, stderr) = repo.breeze_err(&["link", "data.txt"]);
        assert_eq!(code, 5);
        assert!(stderr.contains("differs from the store copy"), "{stderr}");
        assert!(stderr.contains("breeze doctor"), "{stderr}");
    }

    #[test]
    fn link_refuses_tracked_directories_and_escaping_paths() {
        let repo = TempRepo::new("link-refuse");
        repo.write("tracked.txt", "tracked\n");
        repo.git(&repo.main, &["add", "tracked.txt"]);
        repo.git(&repo.main, &["commit", "-m", "track"]);

        let (code, stderr) = repo.breeze_err(&["link", "tracked.txt"]);
        assert_eq!(code, 5);
        assert!(stderr.contains("tracked by git"), "{stderr}");

        std::fs::create_dir_all(repo.main.join("a-directory")).unwrap();
        let (code, _) = repo.breeze_err(&["link", "a-directory"]);
        assert_eq!(code, 5);

        for path in [
            "..",
            "../outside.txt",
            "a/../outside.txt",
            "a/../../outside.txt",
            "./../outside.txt",
        ] {
            let (code, stderr) = repo.breeze_err(&["link", path]);
            assert_eq!(code, 5, "{path}: {stderr}");
            assert!(stderr.contains(".."), "{path}: {stderr}");
        }

        let (code, _) = repo.breeze_err(&["link", "does-not-exist.txt"]);
        assert_eq!(code, 2);
    }

    #[test]
    fn link_supports_branch_and_worktree_layers() {
        let repo = TempRepo::new("link-layers");
        repo.write("branch.txt", "branch\n");
        repo.breeze_ok(&["link", "branch.txt", "--layer", "branch"]);

        let branch_store = repo
            .store_dir()
            .join("branches")
            .join(repo.branch_id(&repo.main))
            .join("files/branch.txt");
        assert!(branch_store.is_file());
        // The feature worktree must not be linked by a branch-layer link.
        assert!(!std::fs::symlink_metadata(repo.other.join("branch.txt")).is_ok());

        repo.write("only-here.txt", "worktree\n");
        repo.breeze_ok(&["link", "only-here.txt", "--layer", "worktree"]);
        assert!(std::fs::symlink_metadata(repo.main.join("only-here.txt"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(std::fs::symlink_metadata(repo.other.join("only-here.txt")).is_err());
    }

    #[test]
    fn link_requires_init() {
        let repo = TempRepo::new("link-uninit");
        let store = repo.store_dir();
        std::fs::remove_dir_all(&store).unwrap();
        repo.write("x.txt", "x\n");
        let (code, stderr) = repo.breeze_err(&["link", "x.txt"]);
        assert_eq!(code, 4);
        assert!(stderr.contains("not initialized"), "{stderr}");
    }
}
