//! `breeze patches` — list active and archived patches.

use anyhow::Result;
use serde_json::json;

use crate::commands::Ctx;
use crate::exit;
use crate::store::overlay::Repo;
use crate::store::overrides::{OverrideEntry, OverrideMode};
use crate::store::patches;

struct Row {
    name: Option<String>,
    path: String,
    layer: String,
    stale: bool,
    archived: bool,
}

pub fn run(ctx: &Ctx) -> Result<i32> {
    let repo = Repo::open(ctx)?;
    let overrides = repo.overrides()?;
    let mut rows = Vec::new();

    for entry in overrides
        .overrides
        .iter()
        .filter(|entry| entry.mode == OverrideMode::Patch)
    {
        rows.push(Row {
            name: entry.name.clone(),
            path: entry.path.clone(),
            layer: entry.layer.clone(),
            stale: active_stale(&repo, entry)?,
            archived: false,
        });
    }

    for dir in repo.all_layer_dirs() {
        for record in patches::list_archived(&dir) {
            let head = patches::head_blob(&repo.info.root, &record.path)?;
            rows.push(Row {
                name: Some(record.name),
                path: record.path,
                layer: record.layer,
                stale: head.as_deref() != Some(record.base_blob.as_str()),
                archived: true,
            });
        }
    }

    rows.sort_by(|left, right| {
        left.archived
            .cmp(&right.archived)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.layer.cmp(&right.layer))
    });

    if ctx.json {
        let value = json!({
            "patches": rows
                .iter()
                .map(|row| json!({
                    "name": row.name,
                    "path": row.path,
                    "layer": row.layer,
                    "stale": row.stale,
                    "archived": row.archived,
                }))
                .collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else if rows.is_empty() {
        println!("no patches recorded");
    } else {
        println!(
            "{:<28} {:<24} {:<9} {:<6} ARCHIVED",
            "NAME", "PATH", "LAYER", "STALE"
        );
        for row in &rows {
            println!(
                "{:<28} {:<24} {:<9} {:<6} {}",
                row.name.as_deref().unwrap_or("(unnamed)"),
                row.path,
                row.layer,
                if row.stale { "yes" } else { "no" },
                if row.archived { "yes" } else { "no" }
            );
        }
    }

    Ok(exit::SUCCESS)
}

fn active_stale(repo: &Repo, entry: &OverrideEntry) -> Result<bool> {
    let patch_file = repo.layout.repo_dir.join(&entry.source);
    let Ok(meta_path) = patches::metadata_path_for(&patch_file) else {
        return Ok(false);
    };
    let Ok(metadata) = patches::read_metadata(&meta_path) else {
        return Ok(false);
    };
    let head = patches::head_blob(&repo.info.root, &entry.path)?;
    Ok(head.as_deref() != Some(metadata.base_blob.as_str()))
}

#[cfg(test)]
mod tests {
    use crate::commands::edit::tests::{commit_file, write_editor};
    use crate::store::overlay::testutil::TempRepo;

    #[test]
    fn patches_lists_active_and_archived_rows() {
        let repo = TempRepo::new("patches-list");
        commit_file(&repo, "fog.json", "base\n");

        let editor = write_editor(&repo, "patches-list-edit", "printf 'local\\n' >> \"$1\"");
        repo.breeze_ok(&[
            "edit",
            "fog.json",
            "--name",
            "api-timeout",
            "--editor",
            &editor,
        ]);

        let text = repo.breeze_ok(&["patches"]);
        assert!(text.contains("api-timeout"), "{text}");
        assert!(text.contains("fog.json"), "{text}");
        assert!(text.contains("branch"), "{text}");

        let json = repo.breeze_ok(&["patches", "--json"]);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let rows = value["patches"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{json}");
        assert_eq!(rows[0]["name"], "api-timeout");
        assert_eq!(rows[0]["path"], "fog.json");
        assert_eq!(rows[0]["layer"], "branch");
        assert_eq!(rows[0]["stale"], false);
        assert_eq!(rows[0]["archived"], false);

        repo.breeze_ok(&["revert", "--all"]);
        let json = repo.breeze_ok(&["patches", "--json"]);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let rows = value["patches"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{json}");
        assert_eq!(rows[0]["name"], "api-timeout");
        assert_eq!(rows[0]["stale"], false);
        assert_eq!(rows[0]["archived"], true);
    }
}
