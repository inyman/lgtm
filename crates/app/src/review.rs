//! Review comments: notes pinned to a file's line span, kept across restarts
//! in the repo's git dir and rendered as a Markdown report to hand back to
//! the agent that wrote the change.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Which numbering a span uses: the working-tree file, or (for spans that
/// only cover removed lines) the base version.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Side {
    New,
    Old,
}

/// Inclusive 1-based line range on one side.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Span {
    pub side: Side,
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn overlaps(&self, other: &Span) -> bool {
        self.side == other.side && self.start <= other.end && other.start <= self.end
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Comment {
    pub path: String,
    pub span: Span,
    pub body: String,
    /// The commented diff lines ("+", "-", " " prefixed) as they were when
    /// the comment was written, so the report stands on its own.
    pub code: String,
}

impl Comment {
    /// `path:190-200` (or `path:190`), the usual file:line form editors and
    /// agents understand; removed-only spans say they use base numbering.
    pub fn location(&self) -> String {
        let Span { side, start, end } = self.span;
        let lines = if start == end {
            format!("{start}")
        } else {
            format!("{start}-{end}")
        };
        match side {
            Side::New => format!("{}:{lines}", self.path),
            Side::Old => format!(
                "{}:{lines} (removed lines, base version numbering)",
                self.path
            ),
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct Review {
    pub comments: Vec<Comment>,
    #[serde(skip)]
    file: Option<PathBuf>,
}

impl Review {
    /// Loads the review stored for `repo_root`, or an empty one bound to
    /// that location. Unreadable or corrupt files start fresh.
    pub fn load(repo_root: &Path) -> Self {
        let file = review_file(repo_root);
        let mut review: Review = file
            .as_ref()
            .and_then(|f| std::fs::read(f).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        review.file = file;
        review
    }

    /// The comment on `path` whose span overlaps `span`.
    pub fn find(&self, path: &str, span: &Span) -> Option<usize> {
        self.comments
            .iter()
            .position(|c| c.path == path && c.span.overlaps(span))
    }

    /// Replaces comment `ix` (or adds a new one) with `comment`; an empty
    /// body deletes it.
    pub fn set(&mut self, ix: Option<usize>, comment: Comment) {
        let empty = comment.body.trim().is_empty();
        match (ix, empty) {
            (Some(ix), true) => {
                self.comments.remove(ix);
            }
            (Some(ix), false) => self.comments[ix] = comment,
            (None, true) => return,
            (None, false) => self.comments.push(comment),
        }
        self.comments
            .sort_by(|a, b| (&a.path, a.span.start).cmp(&(&b.path, b.span.start)));
        self.save();
    }

    /// Drops the comments on `paths`, e.g. once they're committed.
    pub fn forget<'a>(&mut self, paths: impl IntoIterator<Item = &'a str>) {
        let paths: Vec<&str> = paths.into_iter().collect();
        self.comments.retain(|c| !paths.contains(&c.path.as_str()));
        self.save();
    }

    pub fn clear(&mut self) {
        self.comments.clear();
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
        let _ = std::fs::write(file.with_extension("md"), self.to_markdown());
    }

    /// The report for the agent: numbered comments, each with its location,
    /// the diff lines it's about, and the note.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        for (i, c) in self.comments.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(&format!("{}. {}\n\n", i + 1, c.location()));
            if !c.code.is_empty() {
                out.push_str("```diff\n");
                out.push_str(c.code.trim_end_matches('\n'));
                out.push_str("\n```\n\n");
            }
            out.push_str(c.body.trim());
            out.push('\n');
        }
        out
    }
}

/// `<git dir>/lgtm/review.json`: inside the git dir so it never shows up in
/// the diff, and per worktree.
fn review_file(repo_root: &Path) -> Option<PathBuf> {
    git::state_dir(repo_root).map(|dir| dir.join("review.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(path: &str, side: Side, start: u32, end: u32, body: &str) -> Comment {
        Comment {
            path: path.into(),
            span: Span { side, start, end },
            body: body.into(),
            code: String::new(),
        }
    }

    #[test]
    fn locations() {
        assert_eq!(
            comment("a.rs", Side::New, 190, 200, "").location(),
            "a.rs:190-200"
        );
        assert_eq!(comment("a.rs", Side::New, 7, 7, "").location(), "a.rs:7");
        assert!(comment("a.rs", Side::Old, 3, 4, "")
            .location()
            .starts_with("a.rs:3-4 (removed"));
    }

    #[test]
    fn set_edits_deletes_and_sorts() {
        let mut r = Review::default();
        r.set(None, comment("b.rs", Side::New, 1, 2, "x"));
        r.set(None, comment("a.rs", Side::New, 9, 9, "y"));
        assert_eq!(r.comments[0].path, "a.rs");
        let span = Span {
            side: Side::New,
            start: 2,
            end: 5,
        };
        let ix = r.find("b.rs", &span);
        assert_eq!(ix, Some(1));
        assert_eq!(
            r.find(
                "b.rs",
                &Span {
                    side: Side::Old,
                    ..span
                }
            ),
            None
        );
        r.set(ix, comment("b.rs", Side::New, 1, 2, "  "));
        assert_eq!(r.comments.len(), 1);
        assert!(r.to_markdown() == "1. a.rs:9\n\ny\n");
    }
}
