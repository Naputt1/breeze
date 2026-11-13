//! `breeze status` — show repository, worktree and overlay status.

use std::path::Path;

use anyhow::Result;

use crate::commands::Ctx;
use crate::exit::{self, CommandError};
use crate::gitinfo::GitInfo;
use crate::store::{self, identity, overrides::OverridesFile};

pub fn run(ctx: &Ctx) -> Result<i32> {
    let info = GitInfo::discover(ctx.repo.as_deref())?;
    let repo_id = identity::repo_id(info.remote.as_deref(), &info.root, &info.common_dir);
    let branch_id = info.branch.as_deref().map(identity::branch_id);
    let worktree_id = identity::worktree_id(&info.root);
    let store = store::Store::discover()?;
    let layout = store.layout(&repo_id, branch_id.as_deref(), &worktree_id);

    if !store.repo_exists(&repo_id) {
        return Err(CommandError::new(
            exit::NOT_INITIALIZED,
            format!(
                "breeze is not initialized for this repository\nstore: {}\nrun `breeze init` to create it",
                layout.repo_dir.display()
            ),
        )
        .into());
    }

    let meta = store.load_meta(&repo_id)?;
    let overrides = OverridesFile::load(&layout.overrides_file)?;
    let (linked, patches) = overrides.counts();

    let name = repo_name(&meta.root);
    let branch = info.branch.as_deref();
    let worktree = info.root.display().to_string();
    let store_path = layout.repo_dir.display().to_string();

    if ctx.json {
        let value = serde_json::json!({
            "initialized": true,
            "repo_id": &meta.repo_id,
            "name": &name,
            "repo_root": &meta.root,
            "branch": branch,
            "worktree": &worktree,
            "store": &store_path,
            "linked": linked,
            "patches": patches,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("repo:      {name} ({})", meta.repo_id);
        println!("branch:    {}", branch.unwrap_or("(detached)"));
        println!("worktree:  {worktree}");
        println!("store:     {store_path}");
        println!(
            "overlays:  {linked} linked, {patches} patch{}",
            if patches == 1 { "" } else { "es" }
        );
    }

    Ok(exit::SUCCESS)
}

fn repo_name(root: &str) -> String {
    Path::new(root)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string())
}
