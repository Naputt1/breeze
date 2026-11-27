//! `breeze status` — show repository, worktree and overlay status.

use std::collections::{btree_map::Entry, BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::Result;
use serde_json::{json, Value};

use crate::commands::Ctx;
use crate::exit;
use crate::store::overlay::{self, Layer, Repo};
use crate::store::overrides::{OverrideEntry, OverrideMode};
use crate::store::patches;

pub fn run(ctx: &Ctx) -> Result<i32> {
    let repo = Repo::open(ctx)?;
    let overrides = repo.overrides()?;

    let linked: Vec<&OverrideEntry> = overrides
        .overrides
        .iter()
        .filter(|entry| entry.mode == OverrideMode::Link)
        .collect();
    let patched: Vec<&OverrideEntry> = overrides
        .overrides
        .iter()
        .filter(|entry| entry.mode == OverrideMode::Patch)
        .collect();

    let stale = stale_patches(&repo, &patched)?;
    let issues = count_issues(&repo, &linked)?;
    let deferred = if crate::store::filters::enabled(&repo.info.root) {
        crate::store::filters::deferred_paths(&repo)?
    } else {
        Vec::new()
    };
    let deferred_set: BTreeSet<String> = deferred.iter().cloned().collect();

    let name = repo_name(&repo.meta.root);
    let store_path = repo.layout.repo_dir.display().to_string();

    if ctx.json {
        let value = json!({
            "schema_version": crate::store::SCHEMA_VERSION,
            "repo_id": &repo.meta.repo_id,
            "name": &name,
            "root": &repo.meta.root,
            "branch": repo.info.branch,
            "worktree": repo.info.root.display().to_string(),
            "store_path": &store_path,
            "linked": linked.iter().map(|entry| view(entry)).collect::<Vec<_>>(),
            "patched": patched.iter().map(|entry| view(entry)).collect::<Vec<_>>(),
            "stale": stale,
            "deferred": deferred,
            "issues": issues,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("repo:      {name} ({})", repo.meta.repo_id);
        println!(
            "branch:    {}",
            repo.info.branch.as_deref().unwrap_or("(detached)")
        );
        println!("worktree:  {}", repo.info.root.display());
        println!("store:     {store_path}");

        let (links, patches) = overrides.counts();
        println!("overlays:  {links} link(s), {patches} patch(es)");

        if !overrides.overrides.is_empty() {
            print_group("Linked", "shared", &linked, &deferred_set);
            print_group("Linked", "branch", &linked, &deferred_set);
            print_group("Linked", "worktree", &linked, &deferred_set);
            print_group("Patches", "shared", &patched, &deferred_set);
            print_group("Patches", "branch", &patched, &deferred_set);
            print_group("Patches", "worktree", &patched, &deferred_set);
        }

        if !deferred.is_empty() {
            println!("Deferred:            {}", deferred.join(", "));
        }
        if !stale.is_empty() {
            let labels: Vec<String> = stale
                .iter()
                .map(|path| {
                    patched
                        .iter()
                        .find(|entry| entry.path == *path)
                        .map(|entry| entry.label())
                        .unwrap_or_else(|| path.clone())
                })
                .collect();
            println!("Stale:               {}", labels.join(", "));
        }
        println!("issues:              {issues}");
    }

    Ok(exit::SUCCESS)
}

fn view(entry: &OverrideEntry) -> Value {
    json!({
        "path": entry.path,
        "mode": match entry.mode {
            OverrideMode::Link => "link",
            OverrideMode::Patch => "patch",
        },
        "layer": entry.layer,
        "source": entry.source,
        "name": entry.name,
    })
}

fn print_group(label: &str, layer: &str, entries: &[&OverrideEntry], deferred: &BTreeSet<String>) {
    let labels: Vec<String> = entries
        .iter()
        .filter(|entry| entry.layer == layer)
        .map(|entry| {
            if deferred.contains(&entry.path) {
                format!("{} [deferred]", entry.label())
            } else {
                entry.label()
            }
        })
        .collect();
    if labels.is_empty() {
        return;
    }
    println!("{label} ({layer}): {}", labels.join(", "));
}

/// Paths whose recorded patch base no longer matches the blob at `HEAD`.
fn stale_patches(repo: &Repo, patches_entries: &[&OverrideEntry]) -> Result<Vec<String>> {
    // Filter mode merges three-way against the current upstream content; a
    // moved blob is expected and reported as `deferred` when it conflicts.
    if crate::store::filters::enabled(&repo.info.root) {
        return Ok(Vec::new());
    }
    let current = repo.current_target();
    let mut best: BTreeMap<&str, &OverrideEntry> = BTreeMap::new();
    for entry in patches_entries {
        if !crate::store::overlay::entry_applies(entry, &current) {
            continue;
        }
        match best.entry(entry.path.as_str()) {
            Entry::Vacant(slot) => {
                slot.insert(entry);
            }
            Entry::Occupied(mut slot) => {
                let current = Layer::from_name(&slot.get().layer);
                if Layer::from_name(&entry.layer) > current {
                    slot.insert(entry);
                }
            }
        }
    }

    let mut stale = Vec::new();
    for (path, entry) in best {
        let patch_file = repo.layout.repo_dir.join(&entry.source);
        let Ok(meta_path) = patches::metadata_path_for(&patch_file) else {
            continue;
        };
        let Ok(metadata) = patches::read_metadata(&meta_path) else {
            continue;
        };
        let head = patches::head_blob(&repo.info.root, path)?;
        if head.as_deref() != Some(metadata.base_blob.as_str()) {
            stale.push(path.to_string());
        }
    }
    Ok(stale)
}

/// Doctor-lite issue count: broken links plus registered worktrees that are gone.
fn count_issues(repo: &Repo, links: &[&OverrideEntry]) -> Result<usize> {
    let mut issues = 0usize;
    let targets = repo.worktree_targets(false);
    let paths: BTreeSet<&str> = links.iter().map(|entry| entry.path.as_str()).collect();

    for path in paths {
        let entries: Vec<&OverrideEntry> = links
            .iter()
            .copied()
            .filter(|entry| entry.path == path)
            .collect();

        for target in &targets {
            let Some(entry) = overlay::best_entry(entries.iter().copied(), target) else {
                continue;
            };
            let store_file = repo.layout.repo_dir.join(&entry.source);
            if !Repo::is_regular_store_file(&store_file) {
                issues += 1;
                continue;
            }

            let link = target.path.join(path);
            let ok = match fs::symlink_metadata(&link) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    overlay::link_points_to(&link, &store_file)
                }
                _ => false,
            };
            if !ok {
                issues += 1;
            }
        }
    }

    for worktree in &repo.info.worktrees {
        if !worktree.bare && !worktree.path.exists() {
            issues += 1;
        }
    }

    Ok(issues)
}

fn repo_name(root: &str) -> String {
    Path::new(root)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::Value;

    use crate::store::overlay::testutil::TempRepo;

    use crate::commands::edit::tests::{commit_file, write_editor};

    #[test]
    fn json_status_lists_overrides_layers_and_counts_issues() {
        let repo = TempRepo::new("status-json");
        repo.write("config/local.toml", "linked\n");
        repo.breeze_ok(&["link", "config/local.toml"]);
        commit_file(&repo, "fog.json", "base\n");
        let editor = write_editor(&repo, "status-json-edit", "printf 'local\\n' >> \"$1\"");
        repo.breeze_ok(&["edit", "fog.json", "--layer", "branch", "--editor", &editor]);

        let output = repo.breeze_ok(&["status", "--json"]);
        let value: Value = serde_json::from_str(&output).expect("status JSON parses");

        assert_eq!(value["schema_version"], 1);
        assert!(value["repo_id"].as_str().is_some());
        assert_eq!(
            value["root"].as_str(),
            Some(repo.main.canonicalize().unwrap().to_str().unwrap())
        );
        assert_eq!(value["branch"], "main");
        assert_eq!(
            value["worktree"].as_str(),
            Some(repo.main.canonicalize().unwrap().to_str().unwrap())
        );
        assert!(value["store_path"].as_str().is_some_and(
            |path| path.ends_with(repo.store_dir().file_name().unwrap().to_str().unwrap())
        ));

        let linked = value["linked"].as_array().expect("linked array");
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0]["path"], "config/local.toml");
        assert_eq!(linked[0]["mode"], "link");
        assert_eq!(linked[0]["layer"], "shared");
        assert_eq!(linked[0]["source"], "shared/files/config/local.toml");

        let patched = value["patched"].as_array().expect("patched array");
        assert_eq!(patched.len(), 1);
        assert_eq!(patched[0]["path"], "fog.json");
        assert_eq!(patched[0]["mode"], "patch");
        assert_eq!(patched[0]["layer"], "branch");

        assert_eq!(value["stale"].as_array().map(Vec::len), Some(0));
        assert_eq!(value["issues"], 0);

        // A broken link in the other worktree is a doctor-lite issue.
        let other = repo.other.join("config/local.toml");
        fs::remove_file(&other).unwrap();
        let output = repo.breeze_ok(&["status", "--json"]);
        let value: Value = serde_json::from_str(&output).expect("status JSON parses");
        assert_eq!(value["issues"], 1);
    }

    #[test]
    fn json_status_reports_stale_patches_and_missing_worktrees() {
        let repo = TempRepo::new("status-stale");
        commit_file(&repo, "fog.json", "base\none\n");
        let editor = write_editor(
            &repo,
            "status-stale-edit",
            "printf 'base\\nlocal\\n' > \"$1\"",
        );
        repo.breeze_ok(&["edit", "fog.json", "--editor", &editor]);

        // Rewrite the commit under the patch: its recorded base is stale now.
        repo.write("fog.json", "base\namended\n");
        repo.git(&repo.main, &["add", "fog.json"]);
        repo.git(&repo.main, &["commit", "--amend", "--no-edit"]);

        fs::remove_dir_all(&repo.other).unwrap();

        let output = repo.breeze_ok(&["status", "--json"]);
        let value: Value = serde_json::from_str(&output).expect("status JSON parses");
        assert_eq!(value["stale"], serde_json::json!(["fog.json"]));
        assert!(
            value["issues"].as_u64().unwrap_or(0) >= 1,
            "missing worktree should be an issue: {value}"
        );
    }

    #[test]
    fn human_status_keeps_a_readable_summary() {
        let repo = TempRepo::new("status-human");
        repo.write("config/local.toml", "linked\n");
        repo.breeze_ok(&["link", "config/local.toml"]);

        let output = repo.breeze_ok(&["status"]);
        assert!(output.contains("repo:"), "{output}");
        assert!(output.contains("branch:    main"), "{output}");
        assert!(
            output.contains("Linked (shared): config/local.toml"),
            "{output}"
        );
        assert!(output.contains("issues:              0"), "{output}");
    }
}
