//! Local git diffs via the `git` CLI: "the PR I'd open from here" — everything
//! since the merge-base with the default branch (committed + staged + unstaged
//! + untracked), as one unified patch for diff-core to parse.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// How many untracked files to inline into the patch before giving up.
const MAX_UNTRACKED_FILES: usize = 200;

#[derive(Debug, Clone)]
pub struct LocalSource {
    pub repo_root: PathBuf,
    pub branch: String,
    /// A user-selected base ref. None means use the repository default
    /// heuristic on refresh.
    pub base_ref: Option<String>,
    /// Human name of the diff base: "origin/main"-style ref when one exists
    /// and shares history with HEAD, otherwise "HEAD" (working-tree-only diff).
    pub base_label: String,
    /// Commit oid of the diff base, captured at resolve time: the merge-base
    /// with the selected base ref, or HEAD itself. None only in a repo with no
    /// commits yet (old side of every file is then absent).
    pub base_oid: Option<String>,
}

/// Resolve a path inside a git repo to its root, current branch, and diff base.
pub fn resolve_local(path: &Path) -> Result<LocalSource> {
    resolve_local_with_base(path, None)
}

/// Resolve a path inside a git repo using a specific base ref when provided.
pub fn resolve_local_with_base(path: &Path, base_ref: Option<&str>) -> Result<LocalSource> {
    let repo_root = PathBuf::from(
        git(path, &["rev-parse", "--show-toplevel"])
            .with_context(|| format!("{} is not inside a git repository", path.display()))?
            .trim(),
    );
    let branch = git(&repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])?
        .trim()
        .to_string();

    if let Some(base_ref) = base_ref {
        let base_ref = base_ref.trim();
        if base_ref == "HEAD" {
            return Ok(LocalSource {
                repo_root,
                branch,
                base_ref: Some(base_ref.to_string()),
                base_label: "HEAD".to_string(),
                base_oid: git(path, &["rev-parse", "HEAD"])
                    .ok()
                    .map(|oid| oid.trim().to_string()),
            });
        }
        let oid = git(&repo_root, &["merge-base", "HEAD", base_ref])
            .with_context(|| format!("{base_ref} does not share history with HEAD"))?;
        return Ok(LocalSource {
            repo_root,
            branch,
            base_ref: Some(base_ref.to_string()),
            base_label: base_ref.to_string(),
            base_oid: Some(oid.trim().to_string()),
        });
    }

    // Default branch: prefer recorded remote HEAD symrefs, then conventional
    // fork/upstream names, then local main/master. A candidate only counts if
    // it shares a merge-base with HEAD; otherwise fall back to HEAD.
    let mut base_oid = None;
    let mut base_label = "HEAD".to_string();
    for cand in default_base_candidates(&repo_root) {
        if cand == branch {
            continue;
        }
        if let Ok(oid) = git(&repo_root, &["merge-base", "HEAD", &cand]) {
            base_oid = Some(oid.trim().to_string());
            base_label = cand;
            break;
        }
    }
    if base_oid.is_none() {
        // Working-tree-only diff; a repo with zero commits has no HEAD oid.
        base_oid = git(&repo_root, &["rev-parse", "HEAD"])
            .ok()
            .map(|oid| oid.trim().to_string());
    }

    Ok(LocalSource {
        repo_root,
        branch,
        base_ref: None,
        base_label,
        base_oid,
    })
}

/// Refs suitable for choosing a local diff base, in UI order.
pub fn list_base_refs(path: &Path) -> Result<Vec<String>> {
    let repo_root = PathBuf::from(
        git(path, &["rev-parse", "--show-toplevel"])
            .with_context(|| format!("{} is not inside a git repository", path.display()))?
            .trim(),
    );
    let branch = git(&repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])?
        .trim()
        .to_string();
    let mut candidates = Vec::new();
    for cand in default_base_candidates(&repo_root) {
        push_unique(&mut candidates, cand);
    }
    push_unique(&mut candidates, "HEAD".to_string());
    let refs = git(
        &repo_root,
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads",
            "refs/remotes",
        ],
    )?;
    for cand in refs.lines().map(str::trim).filter(|cand| !cand.is_empty()) {
        if cand == branch || cand.ends_with("/HEAD") {
            continue;
        }
        push_unique(&mut candidates, cand.to_string());
    }
    Ok(candidates
        .into_iter()
        .filter(|cand| cand == "HEAD" || git(&repo_root, &["merge-base", "HEAD", cand]).is_ok())
        .collect())
}

fn default_base_candidates(repo_root: &Path) -> Vec<String> {
    let mut candidates = Vec::new();
    push_remote_head(repo_root, "origin", &mut candidates);
    push_remote_head(repo_root, "upstream", &mut candidates);
    push_unique(&mut candidates, "origin/main".to_string());
    push_unique(&mut candidates, "upstream/main".to_string());
    push_unique(&mut candidates, "origin/master".to_string());
    push_unique(&mut candidates, "upstream/master".to_string());
    push_unique(&mut candidates, "main".to_string());
    push_unique(&mut candidates, "master".to_string());
    candidates
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

fn push_remote_head(repo_root: &Path, remote: &str, candidates: &mut Vec<String>) {
    if let Ok(symref) = git(
        repo_root,
        &["symbolic-ref", &format!("refs/remotes/{remote}/HEAD")],
    ) {
        if let Some(name) = symref.trim().strip_prefix("refs/remotes/") {
            push_unique(candidates, name.to_string());
        }
    }
}

/// Full contents of `path` at the captured diff base, for the Phase-2 upgrade.
/// None means "old side absent or unusable": untracked/added files, binary or
/// non-UTF-8 content, or no base commit. Errors collapse to None too — the
/// caller keeps that file's patch-derived view.
pub fn file_at_base(src: &LocalSource, path: &str) -> Option<String> {
    let oid = src.base_oid.as_deref()?;
    let output = git_cmd(&src.repo_root)
        .args(["show", &format!("{oid}:{path}")])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Unified patch of everything that would go into a PR opened from here:
/// merge-base(HEAD, base)..working-tree (two-dot, so committed + staged +
/// unstaged), plus untracked files appended as added-file diffs.
pub fn diff_patch(src: &LocalSource) -> Result<String> {
    // The oid captured at resolve time; "HEAD" only in a zero-commit repo,
    // where the committed-diff half is empty anyway.
    let base = src.base_oid.clone().unwrap_or_else(|| "HEAD".to_string());
    let mut patch = git(
        &src.repo_root,
        &["diff", "-M", "--no-color", "--no-ext-diff", &base],
    )?;

    let untracked = git(
        &src.repo_root,
        &["ls-files", "--others", "--exclude-standard"],
    )?;
    for file in untracked.lines().take(MAX_UNTRACKED_FILES) {
        // `--no-index` against /dev/null renders an untracked file as an
        // added-file diff; it exits 1 when the sides differ, which is success
        // here (0 would mean an empty file — also fine, git emits a header).
        let output = git_cmd(&src.repo_root)
            .args([
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-index",
                "--",
                "/dev/null",
            ])
            .arg(file)
            .output()
            .map_err(|err| anyhow!("failed to run git: {err}"))?;
        if !matches!(output.status.code(), Some(0) | Some(1)) {
            bail!(
                "git diff --no-index /dev/null {file} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        patch.push_str(&String::from_utf8_lossy(&output.stdout));
    }
    Ok(patch)
}

/// What a changed path must hold in a commit for it to be the version that
/// was reviewed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expected {
    /// A regular file with this blob id.
    Blob(String),
    /// Deleted from the working tree.
    Deleted,
    /// Not fingerprinted (symlinks, submodules): committed as found.
    Unverified,
}

/// Fingerprints `paths` in the working tree as `git add` would store them,
/// without writing anything (`hash-object` without `-w`). A path that
/// changes mid-read is left out, so a commit of it is refused until the next
/// read.
pub fn worktree_state(repo_root: &Path, paths: &[String]) -> HashMap<String, Expected> {
    use std::io::Write;
    use std::process::Stdio;
    let mut out = HashMap::new();
    let mut files = Vec::new();
    for path in paths {
        match std::fs::symlink_metadata(repo_root.join(path)) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                out.insert(path.clone(), Expected::Deleted);
            }
            Ok(meta) if meta.is_file() => files.push(path.clone()),
            _ => {
                out.insert(path.clone(), Expected::Unverified);
            }
        }
    }
    if files.is_empty() {
        return out;
    }
    let child = git_cmd(repo_root)
        .args(["hash-object", "--stdin-paths"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return out;
    };
    if let Some(mut stdin) = child.stdin.take() {
        for path in &files {
            let _ = writeln!(stdin, "{path}");
        }
    }
    let Ok(output) = child.wait_with_output() else {
        return out;
    };
    let oids: Vec<&str> = std::str::from_utf8(&output.stdout)
        .unwrap_or_default()
        .lines()
        .collect();
    if output.status.success() && oids.len() == files.len() {
        for (path, oid) in files.into_iter().zip(oids) {
            out.insert(path, Expected::Blob(oid.to_string()));
        }
    }
    out
}

#[derive(Debug)]
pub struct Committed {
    /// `<short hash> <subject>`.
    pub summary: String,
    /// Set when the commit landed but git's staging area couldn't be
    /// brought up to date for its files.
    pub warning: Option<String>,
}

/// Commits exactly `files` (each at its reviewed state) on top of HEAD,
/// without touching anything else: the commit is built in a private index
/// under `.git/lgtm/`, checked against `files`, and HEAD only moves if no
/// other commit landed meanwhile. Afterwards the real staging area is reset
/// for these paths only, as `git commit -- <paths>` would. Hooks don't run.
pub fn commit_files(
    repo_root: &Path,
    files: &[(String, Expected)],
    message: &str,
) -> Result<Committed> {
    if files.is_empty() {
        bail!("nothing to commit");
    }
    let dir = state_dir(repo_root).context("couldn't locate the git directory")?;
    std::fs::create_dir_all(&dir)?;
    let index = dir.join("commit-index");
    let _ = std::fs::remove_file(&index);
    let result = build_commit(repo_root, &index, files, message);
    let _ = std::fs::remove_file(&index);
    let (commit, parent) = result?;

    // Move HEAD (its branch) only if it still points where we started.
    let old = parent.as_deref().unwrap_or("");
    let subject = message.lines().next().unwrap_or_default();
    git(
        repo_root,
        &[
            "update-ref",
            "-m",
            &format!("commit: {subject}"),
            "HEAD",
            &commit,
            old,
        ],
    )
    .map_err(|_| {
        anyhow!("another commit landed while committing; nothing was committed, try again")
    })?;

    let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
    let mut warning = None;
    for attempt in 0..5 {
        let mut args = vec!["reset", "-q", "HEAD", "--"];
        args.extend(&paths);
        match git_literal(repo_root, None, &args) {
            Ok(_) => {
                warning = None;
                break;
            }
            Err(err) => {
                warning = Some(format!(
                    "committed, but git's staging area is busy; run `git reset -q -- <files>` \
                     ({err:#})"
                ));
                std::thread::sleep(std::time::Duration::from_millis(100 * (attempt + 1)));
            }
        }
    }
    let summary = git(repo_root, &["log", "-1", "--format=%h %s", &commit])?
        .trim()
        .to_string();
    Ok(Committed { summary, warning })
}

/// Builds the commit object in the private `index`; returns it and its
/// parent (None on an unborn branch).
fn build_commit(
    repo_root: &Path,
    index: &Path,
    files: &[(String, Expected)],
    message: &str,
) -> Result<(String, Option<String>)> {
    let parent = git(repo_root, &["rev-parse", "--verify", "-q", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string());
    match &parent {
        Some(head) => git_literal(repo_root, Some(index), &["read-tree", head])?,
        None => git_literal(repo_root, Some(index), &["read-tree", "--empty"])?,
    };
    let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
    let mut add = vec!["add", "-A", "--"];
    add.extend(&paths);
    git_literal(repo_root, Some(index), &add)
        .map_err(|err| anyhow!("files changed since review; refresh and retry ({err:#})"))?;

    // What went in must be what was reviewed.
    let mut ls = vec!["ls-files", "-s", "-z", "--"];
    ls.extend(&paths);
    let listed = git_literal(repo_root, Some(index), &ls)?;
    let staged: HashMap<&str, &str> = listed
        .split('\0')
        .filter_map(|entry| {
            let (meta, path) = entry.split_once('\t')?;
            Some((path, meta.split(' ').nth(1)?))
        })
        .collect();
    let changed: Vec<&str> = files
        .iter()
        .filter(|(path, expected)| match expected {
            Expected::Blob(oid) => staged.get(path.as_str()) != Some(&oid.as_str()),
            Expected::Deleted => staged.contains_key(path.as_str()),
            Expected::Unverified => false,
        })
        .map(|(path, _)| path.as_str())
        .collect();
    if !changed.is_empty() {
        bail!(
            "changed since review, nothing was committed: {}",
            changed.join(", ")
        );
    }

    let tree = git_literal(repo_root, Some(index), &["write-tree"])?
        .trim()
        .to_string();
    let mut args = vec!["commit-tree", tree.as_str()];
    if let Some(head) = &parent {
        args.extend(["-p", head.as_str()]);
    }
    let commit = git_stdin(repo_root, &args, message)?.trim().to_string();
    Ok((commit, parent))
}

/// `.git/lgtm` (per worktree): lgtm's own state, never part of the diff.
pub fn state_dir(repo_root: &Path) -> Option<PathBuf> {
    let dir = git(repo_root, &["rev-parse", "--absolute-git-dir"]).ok()?;
    Some(Path::new(dir.trim()).join("lgtm"))
}

/// Whether any of `paths` (inside `repo_root`) is not gitignored — i.e. a
/// change there could show up in the diff. Errs toward `true` if git fails.
pub fn any_unignored(repo_root: &Path, paths: &[PathBuf]) -> bool {
    use std::io::Write;
    use std::process::Stdio;
    if paths.is_empty() {
        return false;
    }
    let child = git_cmd(repo_root)
        .args(["check-ignore", "--stdin", "-z"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return true;
    };
    if let Some(mut stdin) = child.stdin.take() {
        for path in paths {
            let _ = stdin.write_all(path.as_os_str().as_encoded_bytes());
            let _ = stdin.write_all(b"\0");
        }
    }
    let Ok(output) = child.wait_with_output() else {
        return true;
    };
    // Exit 1 means none were ignored; anything but 0/1 is an error.
    match output.status.code() {
        Some(0) => {
            let ignored = output
                .stdout
                .split(|b| *b == 0)
                .filter(|p| !p.is_empty())
                .count();
            ignored < paths.len()
        }
        _ => true,
    }
}

/// A git command in `dir` that never takes optional locks: reads don't
/// refresh the index behind a concurrently running agent's back.
fn git_cmd(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).env("GIT_OPTIONAL_LOCKS", "0");
    cmd
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    run(git_cmd(dir).args(args), args)
}

/// `git` with pathspecs taken literally, optionally against a private index.
fn git_literal(dir: &Path, index: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut cmd = git_cmd(dir);
    cmd.env("GIT_LITERAL_PATHSPECS", "1");
    if let Some(index) = index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    run(cmd.args(args), args)
}

/// `git` with `input` on stdin.
fn git_stdin(dir: &Path, args: &[&str], input: &str) -> Result<String> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = git_cmd(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| anyhow!("failed to run git (is git installed?): {err}"))?;
    child
        .stdin
        .take()
        .context("git stdin")?
        .write_all(input.as_bytes())?;
    output_of(child.wait_with_output()?, args)
}

fn run(cmd: &mut Command, args: &[&str]) -> Result<String> {
    let output = cmd
        .output()
        .map_err(|err| anyhow!("failed to run git (is git installed?): {err}"))?;
    output_of(output, args)
}

fn output_of(output: std::process::Output, args: &[&str]) -> Result<String> {
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use diff_core::FileStatus;
    use std::fs;

    #[test]
    fn any_unignored_respects_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        init_repo(&dir);
        fs::write(dir.join(".gitignore"), "dist/\n").unwrap();
        fs::create_dir_all(dir.join("dist")).unwrap();
        let dist = dir.join("dist/a.js");
        let src = dir.join("src.rs");
        assert!(!any_unignored(&dir, std::slice::from_ref(&dist)));
        assert!(any_unignored(&dir, &[dist, src]));
        assert!(!any_unignored(&dir, &[]));
    }

    fn run(dir: &Path, args: &[&str]) {
        let output = Command::new(args[0])
            .args(&args[1..])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo(dir: &Path) {
        run(dir, &["git", "init", "-b", "main"]);
        run(dir, &["git", "config", "user.email", "test@example.com"]);
        run(dir, &["git", "config", "user.name", "Test"]);
        run(dir, &["git", "config", "commit.gpgsign", "false"]);
    }

    fn state(dir: &Path, paths: &[&str]) -> Vec<(String, Expected)> {
        let paths: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
        let states = worktree_state(dir, &paths);
        paths
            .into_iter()
            .map(|p| {
                let e = states[&p].clone();
                (p, e)
            })
            .collect()
    }

    fn out(dir: &Path, args: &[&str]) -> String {
        git(dir, args).unwrap()
    }

    /// Committing a bucket leaves everything outside it alone: another
    /// file's staged change stays staged, unrelated edits stay unstaged.
    #[test]
    fn commit_files_touches_only_its_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        for f in ["a.rs", "b.rs", "c.rs", "gone.rs", "old.rs"] {
            fs::write(dir.join(f), format!("// {f}\n")).unwrap();
        }
        run(dir, &["git", "add", "."]);
        run(dir, &["git", "commit", "-m", "init"]);

        fs::write(dir.join("a.rs"), "// a v2\n").unwrap(); // bucket
        fs::write(dir.join("new.rs"), "// new\n").unwrap(); // bucket, untracked
        fs::remove_file(dir.join("gone.rs")).unwrap(); // bucket, deleted
        run(dir, &["git", "mv", "old.rs", "moved.rs"]); // bucket, rename
        fs::write(dir.join("b.rs"), "// b staged\n").unwrap();
        run(dir, &["git", "add", "b.rs"]); // staged, not in bucket
        fs::write(dir.join("c.rs"), "// c v2\n").unwrap(); // not in bucket

        let files = state(dir, &["a.rs", "new.rs", "gone.rs", "old.rs", "moved.rs"]);
        let done = commit_files(dir, &files, "Bucket one\n\nbody").unwrap();
        assert!(done.summary.ends_with(" Bucket one"), "{}", done.summary);
        assert!(done.warning.is_none());

        let shown = out(dir, &["show", "--name-status", "--format=", "HEAD"]);
        let mut lines: Vec<&str> = shown.lines().collect();
        lines.sort();
        assert_eq!(
            lines,
            [
                "A\tnew.rs",
                "D\tgone.rs",
                "M\ta.rs",
                "R100\told.rs\tmoved.rs"
            ]
        );
        let status = out(dir, &["status", "--porcelain"]);
        let mut status: Vec<&str> = status.lines().collect();
        status.sort();
        assert_eq!(status, [" M c.rs", "M  b.rs"]);
    }

    #[test]
    fn commit_files_refuses_what_changed_after_review() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        fs::write(dir.join("a.rs"), "// a\n").unwrap();
        run(dir, &["git", "add", "."]);
        run(dir, &["git", "commit", "-m", "init"]);
        let head = out(dir, &["rev-parse", "HEAD"]);

        fs::write(dir.join("a.rs"), "// reviewed\n").unwrap();
        let files = state(dir, &["a.rs"]);
        fs::write(dir.join("a.rs"), "// agent kept going\n").unwrap();
        let err = commit_files(dir, &files, "x").unwrap_err();
        assert!(
            format!("{err:#}").contains("changed since review"),
            "{err:#}"
        );
        assert_eq!(out(dir, &["rev-parse", "HEAD"]), head);
        assert_eq!(out(dir, &["status", "--porcelain"]), " M a.rs\n");
    }

    #[test]
    fn commit_files_on_unborn_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        fs::write(dir.join("a.rs"), "// a\n").unwrap();
        fs::write(dir.join("b.rs"), "// b\n").unwrap();
        commit_files(dir, &state(dir, &["a.rs"]), "first").unwrap();
        assert_eq!(out(dir, &["ls-files"]), "a.rs\n");
        assert_eq!(out(dir, &["status", "--porcelain"]), "?? b.rs\n");
    }

    #[test]
    fn no_remote_main_branch_diffs_working_tree_against_head() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        fs::write(dir.join("a.rs"), "fn main() {}\n").unwrap();
        run(dir, &["git", "add", "."]);
        run(dir, &["git", "commit", "-m", "init"]);

        // Unstaged edit + untracked text file + untracked binary file.
        fs::write(dir.join("a.rs"), "fn main() { println!(); }\n").unwrap();
        fs::write(dir.join("new.txt"), "hello\n").unwrap();
        fs::write(dir.join("blob.bin"), [0u8, 159, 146, 150]).unwrap();

        let src = resolve_local(dir).unwrap();
        assert_eq!(
            src.repo_root.canonicalize().unwrap(),
            dir.canonicalize().unwrap()
        );
        assert_eq!(src.branch, "main");
        assert_eq!(src.base_label, "HEAD");

        let patch = diff_patch(&src).unwrap();
        let diff = diff_core::parse_patch(&patch);
        let by_path: Vec<(&str, FileStatus)> = diff
            .files
            .iter()
            .map(|f| (f.display_path(), f.status))
            .collect();
        assert!(
            by_path.contains(&("a.rs", FileStatus::Modified)),
            "{by_path:?}"
        );
        assert!(
            by_path.contains(&("new.txt", FileStatus::Added)),
            "{by_path:?}"
        );
        assert!(
            by_path.contains(&("blob.bin", FileStatus::Binary)),
            "{by_path:?}"
        );
    }

    #[test]
    fn no_remote_feature_branch_diffs_against_local_main() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        fs::write(dir.join("lib.rs"), "pub fn one() {}\n").unwrap();
        run(dir, &["git", "add", "."]);
        run(dir, &["git", "commit", "-m", "init"]);
        run(dir, &["git", "checkout", "-b", "feature"]);
        fs::write(dir.join("lib.rs"), "pub fn one() {}\npub fn two() {}\n").unwrap();
        run(dir, &["git", "commit", "-am", "add two"]);

        let src = resolve_local(dir).unwrap();
        assert_eq!(src.branch, "feature");
        assert_eq!(src.base_label, "main");

        let patch = diff_patch(&src).unwrap();
        let diff = diff_core::parse_patch(&patch);
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].display_path(), "lib.rs");
        assert_eq!((diff.files[0].additions, diff.files[0].deletions), (1, 0));
    }

    #[test]
    fn explicit_base_ref_overrides_default_base() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        fs::write(dir.join("lib.rs"), "pub fn one() {}\n").unwrap();
        run(dir, &["git", "add", "."]);
        run(dir, &["git", "commit", "-m", "init"]);
        run(dir, &["git", "checkout", "-b", "release"]);
        fs::write(dir.join("lib.rs"), "pub fn one() {}\npub fn release() {}\n").unwrap();
        run(dir, &["git", "commit", "-am", "release"]);
        run(dir, &["git", "checkout", "main"]);
        run(dir, &["git", "checkout", "-b", "feature"]);
        fs::write(dir.join("lib.rs"), "pub fn one() {}\npub fn feature() {}\n").unwrap();
        run(dir, &["git", "commit", "-am", "feature"]);

        let auto = resolve_local(dir).unwrap();
        assert_eq!(auto.base_label, "main");
        assert_eq!(auto.base_ref, None);

        let explicit = resolve_local_with_base(dir, Some("release")).unwrap();
        assert_eq!(explicit.base_label, "release");
        assert_eq!(explicit.base_ref.as_deref(), Some("release"));
        assert_eq!(
            explicit.base_oid.as_deref(),
            Some(git(dir, &["merge-base", "HEAD", "release"]).unwrap().trim())
        );
    }

    #[test]
    fn list_base_refs_includes_head_and_local_branches() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        fs::write(dir.join("lib.rs"), "pub fn one() {}\n").unwrap();
        run(dir, &["git", "add", "."]);
        run(dir, &["git", "commit", "-m", "init"]);
        run(dir, &["git", "checkout", "-b", "release"]);
        run(dir, &["git", "checkout", "main"]);
        run(dir, &["git", "checkout", "-b", "feature"]);

        let refs = list_base_refs(dir).unwrap();
        assert!(refs.iter().any(|base| base == "HEAD"), "{refs:?}");
        assert!(refs.iter().any(|base| base == "main"), "{refs:?}");
        assert!(refs.iter().any(|base| base == "release"), "{refs:?}");
        assert!(!refs.iter().any(|base| base == "feature"), "{refs:?}");
    }

    #[test]
    fn branch_diffs_against_origin_default_head() {
        let tmp = tempfile::tempdir().unwrap();
        let upstream = tmp.path().join("upstream");
        fs::create_dir(&upstream).unwrap();
        init_repo(&upstream);
        fs::write(upstream.join("lib.rs"), "pub fn one() {}\n").unwrap();
        run(&upstream, &["git", "add", "."]);
        run(&upstream, &["git", "commit", "-m", "init"]);

        let clone = tmp.path().join("clone");
        run(
            tmp.path(),
            &[
                "git",
                "clone",
                upstream.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        run(&clone, &["git", "config", "user.email", "test@example.com"]);
        run(&clone, &["git", "config", "user.name", "Test"]);
        run(&clone, &["git", "config", "commit.gpgsign", "false"]);
        run(&clone, &["git", "checkout", "-b", "feature"]);
        fs::write(clone.join("lib.rs"), "pub fn one() {}\npub fn two() {}\n").unwrap();
        run(&clone, &["git", "commit", "-am", "add two"]);
        // Plus an uncommitted edit on top: two-dot diff must include it.
        fs::write(
            clone.join("lib.rs"),
            "pub fn one() {}\npub fn two() {}\npub fn three() {}\n",
        )
        .unwrap();

        let src = resolve_local(&clone).unwrap();
        assert_eq!(src.branch, "feature");
        assert_eq!(src.base_label, "origin/main");

        let patch = diff_patch(&src).unwrap();
        let diff = diff_core::parse_patch(&patch);
        assert_eq!(diff.files.len(), 1);
        let file = &diff.files[0];
        assert_eq!(file.display_path(), "lib.rs");
        assert_eq!(file.status, FileStatus::Modified);
        // Committed line + uncommitted line, both present.
        assert_eq!((file.additions, file.deletions), (2, 0));
    }

    #[test]
    fn fork_branch_diffs_against_upstream_main_when_origin_main_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let upstream = tmp.path().join("upstream");
        fs::create_dir(&upstream).unwrap();
        init_repo(&upstream);
        fs::write(upstream.join("lib.rs"), "pub fn one() {}\n").unwrap();
        run(&upstream, &["git", "add", "."]);
        run(&upstream, &["git", "commit", "-m", "init"]);

        let fork = tmp.path().join("fork");
        run(
            tmp.path(),
            &[
                "git",
                "clone",
                upstream.to_str().unwrap(),
                fork.to_str().unwrap(),
            ],
        );
        run(&fork, &["git", "remote", "rename", "origin", "upstream"]);
        let empty_origin = tmp.path().join("origin");
        fs::create_dir(&empty_origin).unwrap();
        init_repo(&empty_origin);
        run(
            &fork,
            &[
                "git",
                "remote",
                "add",
                "origin",
                empty_origin.to_str().unwrap(),
            ],
        );
        run(&fork, &["git", "config", "user.email", "test@example.com"]);
        run(&fork, &["git", "config", "user.name", "Test"]);
        run(&fork, &["git", "config", "commit.gpgsign", "false"]);
        run(&fork, &["git", "checkout", "-b", "feature"]);
        fs::write(fork.join("lib.rs"), "pub fn one() {}\npub fn two() {}\n").unwrap();
        run(&fork, &["git", "commit", "-am", "add two"]);

        let src = resolve_local(&fork).unwrap();
        assert_eq!(src.branch, "feature");
        assert_eq!(src.base_label, "upstream/main");
    }

    #[test]
    fn file_at_base_reads_committed_content() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        fs::write(dir.join("a.rs"), "fn main() {}\n").unwrap();
        fs::write(dir.join("blob.bin"), [0u8, 159, 146, 150]).unwrap();
        run(dir, &["git", "add", "."]);
        run(dir, &["git", "commit", "-m", "init"]);
        fs::write(dir.join("a.rs"), "fn main() { changed(); }\n").unwrap();
        fs::write(dir.join("new.txt"), "untracked\n").unwrap();

        let src = resolve_local(dir).unwrap();
        // HEAD base still captures a concrete oid.
        assert_eq!(src.base_label, "HEAD");
        assert!(src.base_oid.is_some());

        // Old side = committed content, not the working tree.
        assert_eq!(
            file_at_base(&src, "a.rs").as_deref(),
            Some("fn main() {}\n")
        );
        // Untracked: absent at base. Binary: non-UTF-8 → None.
        assert_eq!(file_at_base(&src, "new.txt"), None);
        assert_eq!(file_at_base(&src, "blob.bin"), None);
        assert_eq!(file_at_base(&src, "no/such/file.rs"), None);
    }

    #[test]
    fn base_oid_is_merge_base_with_remote_head() {
        let tmp = tempfile::tempdir().unwrap();
        let upstream = tmp.path().join("upstream");
        fs::create_dir(&upstream).unwrap();
        init_repo(&upstream);
        fs::write(upstream.join("lib.rs"), "pub fn one() {}\n").unwrap();
        run(&upstream, &["git", "add", "."]);
        run(&upstream, &["git", "commit", "-m", "init"]);

        let clone = tmp.path().join("clone");
        run(
            tmp.path(),
            &[
                "git",
                "clone",
                upstream.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        run(&clone, &["git", "config", "user.email", "test@example.com"]);
        run(&clone, &["git", "config", "user.name", "Test"]);
        run(&clone, &["git", "config", "commit.gpgsign", "false"]);
        run(&clone, &["git", "checkout", "-b", "feature"]);
        fs::write(clone.join("lib.rs"), "pub fn one() {}\npub fn two() {}\n").unwrap();
        run(&clone, &["git", "commit", "-am", "add two"]);

        let src = resolve_local(&clone).unwrap();
        assert_eq!(src.base_label, "origin/main");
        let expected = git(&clone, &["merge-base", "HEAD", "origin/main"]).unwrap();
        assert_eq!(src.base_oid.as_deref(), Some(expected.trim()));
        // Old side comes from the merge-base commit, before the feature edit.
        assert_eq!(
            file_at_base(&src, "lib.rs").as_deref(),
            Some("pub fn one() {}\n")
        );
    }

    #[test]
    fn resolve_rejects_non_repo() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(resolve_local(tmp.path()).is_err());
    }
}
