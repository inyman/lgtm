//! Buckets: a view-only grouping of changed files into named sets, each
//! committable on its own. A bucket holds the files its PATH PATTERNS match
//! (`docs/context`, `*.glb`, `__generated__`) plus the ones moved into it by
//! hand; a file no bucket claims is Unsorted. Every file sits in exactly one
//! place: its hand assignment, else the first bucket (in order) whose pattern
//! matches, else Unsorted. Viewed files leave their bucket for Viewed until
//! their diff changes.
//!
//! Two files, nothing here touches git:
//! - the DEFINITIONS (names, order, patterns) are the repo's policy, shared by
//!   every worktree: `<common git dir>/lgtm/bucket-defs.json`;
//! - the hand ASSIGNMENTS (+ the diff each was sorted at) belong to one
//!   worktree's changes: `<git dir>/lgtm/bucket-files.json`.
//!
//! Both are first seeded from the older per-worktree `buckets.json` (names +
//! assignments) and `filters.json` (the Filtered out patterns, which become a
//! `generated` bucket), which are left in place.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Which files the sidebar and diff show, and what Commit commits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selected {
    /// Files no bucket claims.
    Unsorted,
    Named(String),
    /// Files marked viewed (their diff unchanged since), whatever their bucket.
    Viewed,
}

/// The patterns a repo's first bucket, `generated`, starts with.
pub const DEFAULT_PATTERNS: &[&str] = &["__generated__", "*.wasm", "*.glb", "*.png"];
/// The bucket the default patterns (or an older `filters.json`'s) seed.
const SEEDED_BUCKET: &str = "generated";
/// Names the sidebar's own entries use.
const RESERVED: &[&str] = &["unsorted", "viewed", "all", "default"];

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bucket {
    pub name: String,
    /// Path patterns whose files belong here (see `pattern_matches`).
    #[serde(default)]
    pub patterns: Vec<String>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Assignments {
    /// Path → bucket name.
    files: BTreeMap<String, String>,
    /// Path → fingerprint of its diff when last sorted (or looked at).
    #[serde(default)]
    sorted: BTreeMap<String, u64>,
}

/// The older per-worktree `buckets.json`.
#[derive(Default, Deserialize)]
struct Legacy {
    #[serde(default)]
    names: Vec<String>,
    #[serde(default)]
    files: BTreeMap<String, String>,
    #[serde(default)]
    sorted: BTreeMap<String, u64>,
}

#[derive(Clone, Default)]
pub struct Buckets {
    /// Bucket definitions in order: the first matching pattern wins.
    pub defs: Vec<Bucket>,
    /// Path → bucket name, by hand (beats every pattern).
    pub files: BTreeMap<String, String>,
    /// Path → fingerprint of its diff when last sorted (or looked at), to
    /// flag hand-sorted files that changed since.
    pub sorted: BTreeMap<String, u64>,
    defs_file: Option<PathBuf>,
    files_file: Option<PathBuf>,
}

fn read_json<T: for<'a> Deserialize<'a>>(file: &Option<PathBuf>) -> Option<T> {
    file.as_ref()
        .and_then(|f| std::fs::read(f).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

/// Best effort, like every lgtm state file: a failed write keeps the change
/// for this session only.
fn write_json<T: Serialize>(file: &Option<PathBuf>, value: &T) {
    let Some(file) = file else {
        return;
    };
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_vec_pretty(value) {
        let _ = std::fs::write(file, json);
    }
}

impl Buckets {
    pub fn load(repo_root: &Path) -> Self {
        let local = git::state_dir(repo_root);
        let defs_file = git::shared_state_dir(repo_root).map(|dir| dir.join("bucket-defs.json"));
        let files_file = local.as_ref().map(|dir| dir.join("bucket-files.json"));
        let legacy: Legacy =
            read_json(&local.as_ref().map(|dir| dir.join("buckets.json"))).unwrap_or_default();
        let defs = read_json::<Vec<Bucket>>(&defs_file).unwrap_or_else(|| {
            let patterns =
                read_json::<Vec<String>>(&local.as_ref().map(|dir| dir.join("filters.json")))
                    .unwrap_or_else(|| DEFAULT_PATTERNS.iter().map(|s| s.to_string()).collect());
            let mut defs: Vec<Bucket> = legacy
                .names
                .iter()
                .map(|name| Bucket {
                    name: name.clone(),
                    patterns: Vec::new(),
                })
                .collect();
            if !defs.iter().any(|def| def.name == SEEDED_BUCKET) {
                defs.push(Bucket {
                    name: SEEDED_BUCKET.to_string(),
                    patterns,
                });
            }
            defs
        });
        let assignments = read_json::<Assignments>(&files_file).unwrap_or(Assignments {
            files: legacy.files,
            sorted: legacy.sorted,
        });
        let buckets = Buckets {
            defs,
            files: assignments.files,
            sorted: assignments.sorted,
            defs_file,
            files_file,
        };
        buckets.save_defs();
        buckets.save_files();
        buckets
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.defs.iter().map(|def| def.name.as_str())
    }

    fn exists(&self, name: &str) -> bool {
        self.defs.iter().any(|def| def.name == name)
    }

    /// The bucket `path` was moved into by hand, if that bucket still exists.
    fn assigned(&self, path: &str) -> Option<&str> {
        self.files
            .get(path)
            .map(String::as_str)
            .filter(|name| self.exists(name))
    }

    /// The bucket `path` is in: its hand assignment, else the first bucket
    /// whose pattern matches; None is Unsorted.
    pub fn bucket_of(&self, path: &str) -> Option<&str> {
        self.assigned(path).or_else(|| {
            self.defs
                .iter()
                .find(|def| def.patterns.iter().any(|pat| pattern_matches(pat, path)))
                .map(|def| def.name.as_str())
        })
    }

    /// Whether `path` belongs under `selected` by bucket (Viewed takes every
    /// file; which of them are viewed is the caller's to check).
    pub fn contains(&self, selected: &Selected, path: &str) -> bool {
        match selected {
            Selected::Unsorted => self.bucket_of(path).is_none(),
            Selected::Named(name) => self.bucket_of(path) == Some(name),
            Selected::Viewed => true,
        }
    }

    /// Adds a bucket; false if the name is empty, reserved or taken.
    pub fn create(&mut self, name: &str) -> bool {
        let name = name.trim();
        let reserved = RESERVED.contains(&name.to_lowercase().as_str());
        if name.is_empty() || reserved || self.exists(name) {
            return false;
        }
        self.defs.push(Bucket {
            name: name.to_string(),
            patterns: Vec::new(),
        });
        self.save_defs();
        true
    }

    /// Removes a bucket; its files go back to Unsorted (or to another
    /// bucket's pattern).
    pub fn delete(&mut self, name: &str) {
        self.defs.retain(|def| def.name != name);
        let files = &mut self.files;
        self.sorted
            .retain(|path, _| files.get(path).is_some_and(|b| b != name));
        files.retain(|_, bucket| bucket != name);
        self.save_defs();
        self.save_files();
    }

    /// Adds `pattern` to bucket `name` (a no-op when it has it already).
    pub fn add_pattern(&mut self, name: &str, pattern: &str) {
        let pattern = pattern.trim();
        let Some(def) = self.defs.iter_mut().find(|def| def.name == name) else {
            return;
        };
        if pattern.is_empty() || def.patterns.iter().any(|p| p == pattern) {
            return;
        }
        def.patterns.push(pattern.to_string());
        self.save_defs();
    }

    pub fn remove_pattern(&mut self, name: &str, ix: usize) {
        let Some(def) = self.defs.iter_mut().find(|def| def.name == name) else {
            return;
        };
        if ix < def.patterns.len() {
            def.patterns.remove(ix);
            self.save_defs();
        }
    }

    /// A copy whose bucket `name` also has `draft` — a pattern still being
    /// typed takes effect before Enter pins it. Nothing is saved.
    pub fn with_draft(&self, name: &str, draft: &str) -> Buckets {
        let mut preview = self.clone();
        preview.defs_file = None;
        preview.files_file = None;
        let draft = draft.trim();
        if let Some(def) = preview.defs.iter_mut().find(|def| def.name == name) {
            if !draft.is_empty() && !def.patterns.iter().any(|p| p == draft) {
                def.patterns.push(draft.to_string());
            }
        }
        preview
    }

    /// Puts `files` (path, diff fingerprint) in bucket `name` by hand (None:
    /// the hand assignment is dropped — back to Unsorted, or to the bucket a
    /// pattern gives it).
    pub fn assign<'a>(
        &mut self,
        files: impl IntoIterator<Item = (&'a str, u64)>,
        name: Option<&str>,
    ) {
        for (path, hash) in files {
            match name {
                Some(name) => {
                    self.files.insert(path.to_string(), name.to_string());
                    self.sorted.insert(path.to_string(), hash);
                }
                None => {
                    self.files.remove(path);
                    self.sorted.remove(path);
                }
            }
        }
        self.save_files();
    }

    /// Records that a hand-sorted file was looked at in its current state,
    /// clearing its "changed since sorted" mark.
    pub fn seen<'a>(&mut self, files: impl IntoIterator<Item = (&'a str, u64)>) {
        let mut changed = false;
        for (path, hash) in files {
            if self.assigned(path).is_some() {
                changed |= self.sorted.insert(path.to_string(), hash) != Some(hash);
            }
        }
        if changed {
            self.save_files();
        }
    }

    /// Whether a hand-sorted file's diff changed since it was sorted.
    pub fn changed_since_sorted(&self, path: &str, hash: u64) -> bool {
        self.assigned(path).is_some() && self.sorted.get(path).is_some_and(|h| *h != hash)
    }

    /// Drops assignments for `paths`, e.g. once they're committed.
    pub fn forget<'a>(&mut self, paths: impl IntoIterator<Item = &'a str>) {
        for path in paths {
            self.files.remove(path);
            self.sorted.remove(path);
        }
        self.save_files();
    }

    fn save_defs(&self) {
        write_json(&self.defs_file, &self.defs);
    }

    fn save_files(&self) {
        write_json(
            &self.files_file,
            &Assignments {
                files: self.files.clone(),
                sorted: self.sorted.clone(),
            },
        );
    }
}

/// Whether `path` matches `pattern`. The glob is tried against every run of
/// whole path segments, so `__generated__` matches any file under a directory
/// of that name, `*.snap` matches by basename, and `*/__generated__/*` also
/// covers a top-level `__generated__/`.
pub fn pattern_matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim_start_matches("./").trim_matches('/');
    let pattern = pattern.trim_start_matches("*/").trim_end_matches("/*");
    if pattern.is_empty() {
        return false;
    }
    let starts = std::iter::once(0).chain(path.match_indices('/').map(|(i, _)| i + 1));
    let ends: Vec<usize> = path
        .match_indices('/')
        .map(|(i, _)| i)
        .chain(std::iter::once(path.len()))
        .collect();
    starts.into_iter().any(|start| {
        ends.iter()
            .filter(|&&end| end > start)
            .any(|&end| glob_match(pattern, &path[start..end]))
    })
}

/// Simple wildcard match: `*` matches any run of characters (including `/`),
/// `?` a single character.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = text.chars().collect();
    let (mut pi, mut si) = (0usize, 0usize);
    let (mut star, mut star_s) = (None, 0usize);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_s = si;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            star_s += 1;
            si = star_s;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn membership_create_assign_delete() {
        let mut b = Buckets::default();
        assert!(b.create("change 1"));
        assert!(!b.create("change 1"));
        assert!(!b.create(" Unsorted "));
        assert!(!b.create("viewed"));
        assert!(!b.create(""));
        b.assign([("a.rs", 1), ("b.rs", 2)], Some("change 1"));
        assert!(!b.changed_since_sorted("a.rs", 1));
        assert!(b.changed_since_sorted("a.rs", 7));
        b.seen([("a.rs", 7), ("c.rs", 3)]);
        assert!(!b.changed_since_sorted("a.rs", 7));
        assert!(!b.sorted.contains_key("c.rs"));
        let one = Selected::Named("change 1".into());
        assert!(b.contains(&one, "a.rs"));
        assert!(!b.contains(&Selected::Unsorted, "a.rs"));
        assert!(b.contains(&Selected::Unsorted, "c.rs"));
        b.assign([("b.rs", 2)], None);
        assert!(!b.changed_since_sorted("b.rs", 9));
        assert!(b.contains(&Selected::Unsorted, "b.rs"));
        b.delete("change 1");
        assert!(b.contains(&Selected::Unsorted, "a.rs"));
        assert!(b.files.is_empty());
        assert!(b.sorted.is_empty());
    }

    #[test]
    fn patterns_claim_files_hand_assignment_wins() {
        let mut b = Buckets::default();
        b.create("context");
        b.create("assets");
        b.add_pattern("context", "docs/context");
        b.add_pattern("assets", "*.glb");
        b.add_pattern("assets", "docs/context"); // overlaps: the first bucket wins
        assert_eq!(b.bucket_of("docs/context/a.json"), Some("context"));
        assert_eq!(b.bucket_of("games/x/ship.glb"), Some("assets"));
        assert_eq!(b.bucket_of("src/main.rs"), None);
        // By hand beats the pattern.
        b.assign([("docs/context/a.json", 1)], Some("assets"));
        assert_eq!(b.bucket_of("docs/context/a.json"), Some("assets"));
        // Dropping the hand assignment gives the file back to its pattern.
        b.assign([("docs/context/a.json", 1)], None);
        assert_eq!(b.bucket_of("docs/context/a.json"), Some("context"));
        b.remove_pattern("context", 0);
        assert_eq!(b.bucket_of("docs/context/a.json"), Some("assets"));
        // A draft previews without touching the real definitions.
        let preview = b.with_draft("context", "src");
        assert_eq!(preview.bucket_of("src/main.rs"), Some("context"));
        assert_eq!(b.bucket_of("src/main.rs"), None);
        assert!(b.contains(&Selected::Viewed, "src/main.rs"));
    }

    #[test]
    fn pattern_forms() {
        let ex = pattern_matches;
        for pat in [
            "__generated__",
            "*/__generated__/*",
            "__generated__/",
            "./__generated__",
        ] {
            assert!(ex(pat, "__generated__/a.ts"), "{pat}");
            assert!(ex(pat, "src/x/__generated__/a.ts"), "{pat}");
            assert!(!ex(pat, "src/generated/a.ts"), "{pat}");
        }
        assert!(ex("*.snap", "tests/__snapshots__/a.snap"));
        assert!(ex("src/gen", "src/gen/a.rs"));
        assert!(!ex("src/gen", "lib/src/gener/a.rs"));
        assert!(!ex("", "a.rs"));
    }
}
