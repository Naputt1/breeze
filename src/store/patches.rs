//! Patch storage, metadata and the small Git plumbing Phase 3 needs.
//!
//! Patches live beside a layer as `<layer>/patches/<relative-path>.patch`
//! with a sibling `<relative-path>.json` describing the base they were made
//! against. The metadata carries hashes, not timestamps, so drift is decided
//! by content.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::store::{read_json, write_json, write_private_file};

/// Contents of `<layer>/patches/<relative-path>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchMetadata {
    /// Repository-relative path the patch applies to.
    pub path: String,
    /// Short `HEAD` sha when the patch was created.
    pub base_sha: String,
    /// Full blob id of the `HEAD` version of the file.
    pub base_blob: String,
    /// `sha256:` hash of the file content after the last successful sync.
    pub applied_hash: String,
    /// Optional human-readable patch name, kept in sync with the override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Sidecar metadata for an archived patch under `<layer>/backups/patches/`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchivedPatch {
    pub name: String,
    /// Repository-relative path the patch applies to.
    pub path: String,
    /// Layer the patch was archived from.
    pub layer: String,
    pub base_sha: String,
    pub base_blob: String,
    pub archived_at: String,
}

/// Address of a patch and its metadata inside one layer.
#[derive(Debug, Clone)]
pub struct PatchFiles {
    pub patch: PathBuf,
    pub metadata: PathBuf,
}

/// Compute the patch and metadata paths for a path inside a layer directory.
pub fn patch_files(layer_dir: &Path, relative: &str) -> PatchFiles {
    let patches_dir = layer_dir.join("patches");
    PatchFiles {
        patch: patches_dir.join(format!("{relative}.patch")),
        metadata: patches_dir.join(format!("{relative}.json")),
    }
}

/// Metadata path that sits next to a stored patch file.
pub fn metadata_path_for(patch_file: &Path) -> Result<PathBuf> {
    let text = patch_file.to_string_lossy();
    let stem = text.strip_suffix(".patch").ok_or_else(|| {
        anyhow::anyhow!(
            "patch file name does not end in .patch: {}",
            patch_file.display()
        )
    })?;
    Ok(PathBuf::from(format!("{stem}.json")))
}

/// Read patch metadata.
pub fn read_metadata(path: &Path) -> Result<PatchMetadata> {
    read_json(path)
}

/// Write patch metadata with private permissions.
pub fn write_metadata(path: &Path, metadata: &PatchMetadata) -> Result<()> {
    write_json(path, metadata)
}

/// Directory holding archived patches for a layer.
pub fn archive_dir(layer_dir: &Path) -> PathBuf {
    layer_dir.join("backups").join("patches")
}

/// Address of an archived patch and its sidecar inside one layer.
pub fn archive_files(layer_dir: &Path, name: &str) -> PatchFiles {
    let dir = archive_dir(layer_dir);
    PatchFiles {
        patch: dir.join(format!("{name}.patch")),
        metadata: dir.join(format!("{name}.json")),
    }
}

/// Read an archived patch sidecar.
pub fn read_archived(metadata: &Path) -> Result<ArchivedPatch> {
    read_json(metadata)
}

/// Write an archived patch and its sidecar with private permissions.
pub fn write_archived(
    layer_dir: &Path,
    record: &ArchivedPatch,
    patch: &[u8],
) -> Result<PatchFiles> {
    let files = archive_files(layer_dir, &record.name);
    write_private_file(&files.patch, patch)?;
    write_json(&files.metadata, record)?;
    Ok(files)
}

/// Sidecars of every archived patch in one layer, skipping unreadable files.
pub fn list_archived(layer_dir: &Path) -> Vec<ArchivedPatch> {
    let dir = archive_dir(layer_dir);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut records = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        if let Ok(record) = read_archived(&path) {
            records.push(record);
        }
    }
    records.sort_by(|left, right| left.name.cmp(&right.name));
    records
}

/// Validate a patch name: `[a-z0-9][a-z0-9._-]{0,63}`.
pub fn validate_name(name: &str) -> Result<()> {
    let valid = name.len() <= 64
        && name
            .bytes()
            .next()
            .map(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            .unwrap_or(false)
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        });

    if valid {
        Ok(())
    } else {
        Err(crate::exit::CommandError::new(
            crate::exit::USAGE,
            format!(
                "invalid patch name `{name}`; use lowercase letters, digits, `.`, `_` or `-`, starting with a letter or digit (at most 64 characters)"
            ),
        )
        .into())
    }
}

/// Name used when a patch is archived without one: the sanitized path plus
/// the first eight hex characters of the patch content hash.
pub fn fallback_name(relative: &str, patch: &[u8]) -> String {
    let mut base = crate::store::identity::sanitize(relative);
    base.truncate(55);
    while base.ends_with('-') {
        base.pop();
    }
    if base.is_empty() {
        base.push_str("patch");
    }
    let hash = sha256_hex(patch);
    format!("{base}-{}", &hash[..8])
}

/// Remove a stored patch and its metadata, ignoring absence.
pub fn remove_patch_files(patch_file: &Path) -> Result<()> {
    remove_if_exists(patch_file)?;
    if let Ok(metadata) = metadata_path_for(patch_file) {
        remove_if_exists(&metadata)?;
    }
    Ok(())
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to remove {}", path.display())),
    }
}

/// Whether the contents look binary (a NUL byte is the same heuristic Git
/// uses for the working tree).
pub fn looks_binary(contents: &[u8]) -> bool {
    contents.contains(&0)
}

/// `sha256:` prefixed hash of `contents`.
pub fn hash_bytes(contents: &[u8]) -> String {
    format!("sha256:{}", sha256_hex(contents))
}

/// `sha256:` prefixed hash of a file, or `None` when it does not exist.
pub fn file_hash(path: &Path) -> Option<String> {
    fs::read(path).ok().map(|contents| hash_bytes(&contents))
}

/// Whether `relative` is tracked as a submodule (mode 160000).
pub fn is_submodule(root: &Path, relative: &str) -> Result<bool> {
    let output = git_output(root, &["ls-files", "--stage", "--", relative])?;
    for line in output.lines() {
        if line.split_whitespace().next() == Some("160000") {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Blob id of `relative` at `HEAD`, when the path exists there.
pub fn head_blob(root: &Path, relative: &str) -> Result<Option<String>> {
    git_stdout(
        root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("HEAD:{relative}"),
        ],
    )
}

/// Short `HEAD` sha.
pub fn short_head(root: &Path) -> Result<Option<String>> {
    git_stdout(root, &["rev-parse", "--short", "HEAD"])
}

/// Whether a blob object still exists in the object database.
pub fn blob_exists(root: &Path, blob: &str) -> Result<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["cat-file", "-e", &format!("{blob}^{{blob}}")])
        .output()
        .context("failed to run git cat-file")?;
    Ok(output.status.success())
}

/// Read a blob's raw contents.
pub fn blob_content(root: &Path, blob: &str) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["cat-file", "blob", blob])
        .output()
        .context("failed to run git cat-file")?;
    if !output.status.success() {
        bail!(
            "failed to read blob {blob}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

/// Generate the stored patch for `relative` against `HEAD`.
///
/// The result is empty when the working-tree file matches `HEAD`.
pub fn generate_diff(root: &Path, relative: &str) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--no-ext-diff", "--binary", "HEAD", "--", relative])
        .output()
        .context("failed to run git diff")?;
    if !output.status.success() {
        bail!(
            "git diff failed for {relative}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Outcome of applying a stored patch in one worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The patch applied without conflicts.
    Clean,
    /// Git wrote conflict markers into the file.
    Conflicted,
}

/// Apply a stored patch in `worktree` with a three-way merge.
///
/// A failed merge that leaves conflict markers in the target file is reported
/// as [`ApplyOutcome::Conflicted`]; every other failure is an error.
pub fn apply_patch(worktree: &Path, patch_file: &Path, relative: &str) -> Result<ApplyOutcome> {
    // `git apply --3way` implies `--index` and refuses to run when the working
    // tree differs from the index. Work on a private copy of the index so
    // unrelated edits are merged against the current file without staging
    // anything in the real worktree.
    let index = copy_index_to_temp(worktree)?;
    let result = apply_with_index(worktree, patch_file, relative, &index);
    let _ = fs::remove_file(&index);
    result
}

fn apply_with_index(
    worktree: &Path,
    patch_file: &Path,
    relative: &str,
    index: &Path,
) -> Result<ApplyOutcome> {
    let add = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .env("GIT_INDEX_FILE", index)
        .args(["add", "--", relative])
        .output()
        .context("failed to run git add")?;
    if !add.status.success() {
        bail!(
            "git add failed for {relative}: {}",
            String::from_utf8_lossy(&add.stderr).trim()
        );
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .env("GIT_INDEX_FILE", index)
        .args(["apply", "--3way", "--whitespace=nowarn"])
        .arg(patch_file)
        .output()
        .context("failed to run git apply")?;

    if output.status.success() {
        return Ok(ApplyOutcome::Clean);
    }

    let target = worktree.join(relative);
    let contents = fs::read(&target).unwrap_or_default();
    if has_conflict_markers(&contents) {
        return Ok(ApplyOutcome::Conflicted);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("git apply failed: {}", stderr.trim())
}

fn copy_index_to_temp(worktree: &Path) -> Result<PathBuf> {
    let index = git_index_path(worktree)?;
    let temp = std::env::temp_dir().join(format!(
        "breeze-index-{}-{}",
        std::process::id(),
        now_nanos()
    ));
    if index.is_file() {
        fs::copy(&index, &temp)
            .with_context(|| format!("failed to copy the index {}", index.display()))?;
    } else {
        fs::File::create(&temp).with_context(|| format!("failed to create {}", temp.display()))?;
    }
    Ok(temp)
}

fn git_index_path(worktree: &Path) -> Result<PathBuf> {
    let raw = git_output(worktree, &["rev-parse", "--git-path", "index"])?;
    let path = PathBuf::from(raw.trim());
    Ok(if path.is_absolute() {
        path
    } else {
        worktree.join(path)
    })
}

fn now_nanos() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

/// Whether a file contains unified-diff conflict markers.
pub fn has_conflict_markers(contents: &[u8]) -> bool {
    contents
        .split(|byte| *byte == b'\n')
        .any(|line| line.starts_with(b"<<<<<<<") || line.starts_with(b">>>>>>>"))
}

fn git_output(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("failed to run git")?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn git_stdout(root: &Path, args: &[&str]) -> Result<Option<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("failed to run git")?;
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let value = text.trim();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value.to_string()))
    }
}

/// Shorten a blob/sha for display.
pub fn short(value: &str) -> &str {
    let end = value.len().min(8);
    &value[..end]
}

/// Minimal SHA-256, so patch hashes need no extra dependency.
pub fn sha256_hex(data: &[u8]) -> String {
    const H0: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    const K: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];

    let mut h = H0;
    let mut message = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    // The message is padded to a multiple of 64 bytes above, so every chunk
    // is full. Avoid chunks_exact here: newer Clippy suggests as_chunks,
    // which is newer than this crate's Rust 1.70 MSRV.
    for chunk in message.chunks(64) {
        let mut w = [0u32; 64];
        for (index, word) in w.iter_mut().take(16).enumerate() {
            let base = index * 4;
            *word = u32::from_be_bytes([
                chunk[base],
                chunk[base + 1],
                chunk[base + 2],
                chunk[base + 3],
            ]);
        }
        for index in 16..64 {
            let s0 = w[index - 15].rotate_right(7)
                ^ w[index - 15].rotate_right(18)
                ^ (w[index - 15] >> 3);
            let s1 = w[index - 2].rotate_right(17)
                ^ w[index - 2].rotate_right(19)
                ^ (w[index - 2] >> 10);
            w[index] = w[index - 16]
                .wrapping_add(s0)
                .wrapping_add(w[index - 7])
                .wrapping_add(s1);
        }

        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];

        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[index])
                .wrapping_add(w[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    h.iter().map(|word| format!("{word:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn metadata_path_replaces_only_the_patch_suffix() {
        let meta = metadata_path_for(Path::new("/store/branches/b/patches/fog.json.patch"))
            .expect("metadata path");
        assert_eq!(
            meta,
            PathBuf::from("/store/branches/b/patches/fog.json.json")
        );
        assert!(metadata_path_for(Path::new("/store/fog.json")).is_err());
    }

    #[test]
    fn detects_binary_and_conflict_markers() {
        assert!(looks_binary(b"a\0b"));
        assert!(!looks_binary(b"plain text\n"));
        assert!(has_conflict_markers(b"a\n<<<<<<< HEAD\nb\n"));
        assert!(!has_conflict_markers(b"a\nb\n"));
    }
}
