//! Buckets: a view-only grouping of changed files into named sets, each
//! committable on its own. Files not assigned to a bucket are in Default;
//! files matching a filter pattern are in Filtered out, whatever their
//! assignment.
//! Kept in `.git/lgtm/buckets.json`; nothing here touches git.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Which files the sidebar and diff show, and what Commit commits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selected {
    /// Everything but Filtered out.
    All,
    Default,
    Filtered,
    Named(String),
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Buckets {
    /// Named buckets in creation order.
    pub names: Vec<String>,
    /// Path → bucket name.
    pub files: BTreeMap<String, String>,
    #[serde(skip)]
    file: Option<PathBuf>,
}

impl Buckets {
    pub fn load(repo_root: &Path) -> Self {
        let file = git::state_dir(repo_root).map(|dir| dir.join("buckets.json"));
        let mut buckets: Buckets = file
            .as_ref()
            .and_then(|f| std::fs::read(f).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        buckets.file = file;
        buckets
    }

    /// The named bucket `path` is in; None is Default. Assignments to a
    /// bucket that no longer exists count as Default.
    pub fn bucket_of(&self, path: &str) -> Option<&str> {
        self.files
            .get(path)
            .map(String::as_str)
            .filter(|name| self.names.iter().any(|n| n == name))
    }

    /// Whether `path` shows under `selected`; `filtered` is whether it
    /// matches a filter pattern.
    pub fn contains(&self, selected: &Selected, path: &str, filtered: bool) -> bool {
        match selected {
            Selected::Filtered => filtered,
            _ if filtered => false,
            Selected::All => true,
            Selected::Default => self.bucket_of(path).is_none(),
            Selected::Named(name) => self.bucket_of(path) == Some(name),
        }
    }

    /// Adds a bucket; false if the name is empty, reserved or taken.
    pub fn create(&mut self, name: &str) -> bool {
        let name = name.trim();
        let reserved = ["all", "default", "filtered out"].contains(&name.to_lowercase().as_str());
        if name.is_empty() || reserved || self.names.iter().any(|n| n == name) {
            return false;
        }
        self.names.push(name.to_string());
        self.save();
        true
    }

    /// Removes a bucket; its files go back to Default.
    pub fn delete(&mut self, name: &str) {
        self.names.retain(|n| n != name);
        self.files.retain(|_, bucket| bucket != name);
        self.save();
    }

    /// Puts `paths` in bucket `name` (None: back to Default).
    pub fn assign<'a>(&mut self, paths: impl IntoIterator<Item = &'a str>, name: Option<&str>) {
        for path in paths {
            match name {
                Some(name) => self.files.insert(path.to_string(), name.to_string()),
                None => self.files.remove(path),
            };
        }
        self.save();
    }

    /// Drops assignments for `paths`, e.g. once they're committed.
    pub fn forget<'a>(&mut self, paths: impl IntoIterator<Item = &'a str>) {
        for path in paths {
            self.files.remove(path);
        }
        self.save();
    }

    fn save(&self) {
        let Some(file) = &self.file else {
            return;
        };
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::write(file, json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn membership_create_assign_delete() {
        let mut b = Buckets::default();
        assert!(b.create("change 1"));
        assert!(!b.create("change 1"));
        assert!(!b.create(" Default "));
        assert!(!b.create(""));
        b.assign(["a.rs", "b.rs"], Some("change 1"));
        let one = Selected::Named("change 1".into());
        assert!(b.contains(&one, "a.rs", false));
        assert!(!b.contains(&Selected::Default, "a.rs", false));
        assert!(b.contains(&Selected::Default, "c.rs", false));
        assert!(b.contains(&Selected::All, "a.rs", false));
        // A pattern match beats the assignment.
        assert!(!b.contains(&one, "a.rs", true));
        assert!(!b.contains(&Selected::All, "a.rs", true));
        assert!(b.contains(&Selected::Filtered, "a.rs", true));
        assert!(!b.contains(&Selected::Filtered, "c.rs", false));
        b.assign(["b.rs"], None);
        assert!(b.contains(&Selected::Default, "b.rs", false));
        b.delete("change 1");
        assert!(b.contains(&Selected::Default, "a.rs", false));
        assert!(b.files.is_empty());
    }
}
