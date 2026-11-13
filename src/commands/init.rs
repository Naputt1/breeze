//! `breeze init` — create the store for the current repository.

use std::path::Path;

use anyhow::Result;

use crate::commands::Ctx;
use crate::exit;
use crate::gitinfo::GitInfo;
use crate::store::{self, identity, RepoMetadata};

pub fn run(ctx: &Ctx) -> Result<i32> {
    let info = GitInfo::discover(ctx.repo.as_deref())?;
    let repo_id = identity::repo_id(
        info.remote.as_deref(),
        &info.main_worktree,
        &info.common_dir,
    );
    let branch_id = info.branch.as_deref().map(identity::branch_id);
    let worktree_id = identity::worktree_id(&info.root);
    let store = store::Store::discover()?;

    if store.repo_exists(&repo_id) {
        if !ctx.quiet {
            println!("already initialized");
            println!("store:     {}", store.repo_dir(&repo_id).display());
        }
        return Ok(exit::SUCCESS);
    }

    let meta = RepoMetadata::new(
        repo_id,
        &info.root,
        &info.common_dir,
        info.remote.clone(),
        &info.main_worktree,
    );
    let layout = store.create_repo(&meta, branch_id.as_deref(), &worktree_id)?;

    if !ctx.quiet {
        println!("initialized breeze");
        println!("repo:      {} ({})", repo_name(&meta.root), meta.repo_id);
        println!(
            "branch:    {}",
            info.branch.as_deref().unwrap_or("(detached)")
        );
        println!("worktree:  {}", info.root.display());
        println!("store:     {}", layout.repo_dir.display());
        println!("remote:    {}", meta.remote.as_deref().unwrap_or("(none)"));
    }

    Ok(exit::SUCCESS)
}

fn repo_name(root: &str) -> String {
    Path::new(root)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string())
}
