//! lgtm — a minimal local git-diff viewer.
//!
//! Open it inside a repository (or pass a path) and it shows everything that
//! changed since the diff base: committed, staged, unstaged, and untracked —
//! as one reviewable diff with syntax highlighting, word-level intra-line
//! diffs, unified/split views, and a file tree.

mod theme;

use diff_core::{DiffRow, FileDiff, FileStatus, PrDiff};
use fuzzy_matcher::{skim::SkimMatcherV2, FuzzyMatcher};
use gpui::{
    actions, div, font, point, prelude::*, px, size, uniform_list, App,
    Application, Bounds, ClipboardItem, Context, FocusHandle, HighlightStyle, Hsla, KeyBinding,
    Keystroke, ListHorizontalSizingBehavior, MouseButton, MouseDownEvent, MouseMoveEvent, ScrollWheelEvent,
    MouseUpEvent, Pixels, Point, ScrollStrategy, SharedString, StyledText, Subscription,
    TitlebarOptions, UniformListScrollHandle, Window, WindowBounds, WindowOptions,
};
use gpui_component::{
    input::{Escape as InputEscape, Input, InputEvent, InputState},
    kbd::Kbd,
    scroll::Scrollbar,
    tag::Tag,
    Root, Sizable as _, TitleBar,
};
use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

const MONO: &str = "JetBrainsMono Nerd Font";

/// Diff pane font size in px, adjustable at runtime (cmd-+ / cmd-- / cmd-0).
static FONT_PX: AtomicU32 = AtomicU32::new(DEFAULT_TEXT_SIZE as u32);
const DEFAULT_TEXT_SIZE: f32 = 15.0;
const MIN_TEXT_SIZE: f32 = 7.0;
const MAX_TEXT_SIZE: f32 = 28.0;
const LINE_HEIGHT_RATIO: f32 = 1.7;

fn text_size() -> f32 {
    FONT_PX.load(Ordering::Relaxed) as f32
}

fn row_height_for(size: f32) -> f32 {
    (size * LINE_HEIGHT_RATIO).round()
}

fn row_height() -> f32 {
    row_height_for(text_size())
}

/// Gutter widths in px, matching render_row's fixed-width children: unified is
/// two 44px line-number columns + a 28px marker; each split cell is one of each.
const UNIFIED_GUTTER: f32 = 44. + 44. + 28.;
const SPLIT_GUTTER: f32 = 44. + 28.;
const SPLIT_DIVIDER: f32 = 6.0;

actions!(
    lgtm,
    [
        NextFile,
        PrevFile,
        NextHunk,
        PrevHunk,
        HunkDown,
        HunkUp,
        TreeUp,
        TreeDown,
        TreeOpen,
        TreeCollapse,
        TreeExpand,
        FocusDiff,
        GoToTop,
        GoToBottom,
        ToggleView,
        Quit,
        ToggleSidebar,
        Refresh,
        ClearSelection,
        CopySelection,
        FocusTreeFilter,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        ToggleKeybindings,
    ]
);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LineKind {
    Context,
    Added,
    Removed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Unified,
    Split,
}

/// One side of a split row: line number, kind, text, word-level highlights,
/// and tree-sitter token spans.
struct Cell {
    no: u32,
    kind: LineKind,
    text: SharedString,
    intra: Vec<Range<usize>>,
    syntax: Vec<(Range<usize>, syntax::Token)>,
}

enum Row {
    Spacer,
    FileHeader {
        path: SharedString,
        old_path: Option<SharedString>,
        status: FileStatus,
        additions: u32,
        deletions: u32,
    },
    HunkHeader {
        label: SharedString,
    },
    Binary,
    Line {
        old_no: Option<u32>,
        new_no: Option<u32>,
        kind: LineKind,
        text: SharedString,
        intra: Vec<Range<usize>>,
        syntax: Vec<(Range<usize>, syntax::Token)>,
    },
    SplitLine {
        left: Option<Cell>,
        right: Option<Cell>,
    },
}

/// Patch-only highlighting: we have no full files, so highlight each hunk's
/// text standalone, per side — old_source is context+removed lines, new_source
/// is context+added — and hand each row its side's line spans.
const MAX_HUNK_SOURCE_BYTES: usize = 100 * 1024;
const MAX_SYNTAX_LINE_BYTES: usize = 4096;

fn hunk_syntax(
    lang: Option<&'static syntax::Language>,
    rows: &[DiffRow],
) -> Vec<Vec<(Range<usize>, syntax::Token)>> {
    let Some(lang) = lang else {
        return vec![Vec::new(); rows.len()];
    };
    let mut old_source = String::new();
    let mut new_source = String::new();
    let mut side_lines = Vec::with_capacity(rows.len());
    let (mut old_line, mut new_line) = (0usize, 0usize);
    for row in rows {
        match row {
            DiffRow::Context { text, .. } => {
                old_source.push_str(text);
                old_source.push('\n');
                old_line += 1;
                new_source.push_str(text);
                new_source.push('\n');
                side_lines.push((false, new_line));
                new_line += 1;
            }
            DiffRow::Added { text, .. } => {
                new_source.push_str(text);
                new_source.push('\n');
                side_lines.push((false, new_line));
                new_line += 1;
            }
            DiffRow::Removed { text, .. } => {
                old_source.push_str(text);
                old_source.push('\n');
                side_lines.push((true, old_line));
                old_line += 1;
            }
        }
    }
    let highlight = |source: &str| {
        if source.is_empty() || source.len() > MAX_HUNK_SOURCE_BYTES {
            Vec::new()
        } else {
            syntax::highlight_lines(lang, source)
        }
    };
    let old_spans = highlight(&old_source);
    let new_spans = highlight(&new_source);
    rows.iter()
        .zip(side_lines)
        .map(|(row, (from_old, line))| {
            let text = match row {
                DiffRow::Context { text, .. }
                | DiffRow::Added { text, .. }
                | DiffRow::Removed { text, .. } => text,
            };
            if text.len() > MAX_SYNTAX_LINE_BYTES {
                return Vec::new();
            }
            let side = if from_old { &old_spans } else { &new_spans };
            side.get(line).cloned().unwrap_or_default()
        })
        .collect()
}

/// Flatten the diff into display rows plus the row indices of file headers and
/// hunk headers. Split mode pairs removed/added runs positionally into
/// two-cell rows; unequal runs leave one-sided rows.
/// Index and char count of the longest line across the display rows. The
/// index is used to tell the uniform list which row to measure for horizontal
/// scrolling; the char count sizes split-mode columns.
fn widest_line(rows: &[Row]) -> (usize, usize) {
    let mut best_ix = 0;
    let mut best_chars = 0;
    for (ix, row) in rows.iter().enumerate() {
        let chars = match row {
            Row::Line { text, .. } => text.chars().count(),
            Row::SplitLine { left, right } => {
                let l = left.as_ref().map(|c| c.text.chars().count()).unwrap_or(0);
                let r = right.as_ref().map(|c| c.text.chars().count()).unwrap_or(0);
                l.max(r)
            }
            _ => 0,
        };
        if chars > best_chars {
            best_chars = chars;
            best_ix = ix;
        }
    }
    (best_ix, best_chars)
}

/// Files matching the sidebar's "filter files…" query, or None when the
/// query is empty (everything shown).
fn shown_files(diff: &PrDiff, query: &str) -> Option<HashSet<usize>> {
    if query.trim().is_empty() {
        return None;
    }
    let paths: Vec<&str> = diff.files.iter().map(|f| f.display_path()).collect();
    Some(fuzzy_file_matches(&paths, query).into_iter().collect())
}

/// `only` limits the rows to those files. `file_rows` still has one entry
/// per file in `diff`; a hidden file points at the row where the next shown
/// file starts (or past the end), so file-index lookups stay valid.
fn build_rows(
    diff: &PrDiff,
    mode: ViewMode,
    only: Option<&HashSet<usize>>,
) -> (Vec<Row>, Vec<usize>, Vec<usize>) {
    let mut rows = Vec::new();
    let mut file_rows = Vec::new();
    let mut hunk_rows = Vec::new();

    for (file_ix, file) in diff.files.iter().enumerate() {
        let path = file.display_path();
        if only.is_some_and(|only| !only.contains(&file_ix)) {
            file_rows.push(rows.len() + usize::from(!rows.is_empty()));
            continue;
        }
        if !rows.is_empty() {
            rows.push(Row::Spacer);
        }
        file_rows.push(rows.len());
        rows.push(Row::FileHeader {
            path: path.to_string().into(),
            old_path: match file.status {
                FileStatus::Renamed => file.old_path.clone().map(Into::into),
                _ => None,
            },
            status: file.status,
            additions: file.additions,
            deletions: file.deletions,
        });
        if file.status == FileStatus::Binary {
            rows.push(Row::Binary);
            continue;
        }
        let lang = syntax::language_for_path(path);
        for hunk in &file.hunks {
            let syntax_spans = hunk_syntax(lang, &hunk.rows);
            hunk_rows.push(rows.len());
            let mut label = format!(
                "@@ -{},{} +{},{} @@",
                hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
            );
            if !hunk.section.is_empty() {
                label.push(' ');
                label.push_str(&hunk.section);
            }
            rows.push(Row::HunkHeader {
                label: label.into(),
            });
            match mode {
                ViewMode::Unified => {
                    for (ix, row) in hunk.rows.iter().enumerate() {
                        let syntax = syntax_spans[ix].clone();
                        rows.push(match row {
                            DiffRow::Context {
                                old_no,
                                new_no,
                                text,
                            } => Row::Line {
                                old_no: Some(*old_no),
                                new_no: Some(*new_no),
                                kind: LineKind::Context,
                                text: text.clone().into(),
                                intra: Vec::new(),
                                syntax,
                            },
                            DiffRow::Added {
                                new_no,
                                text,
                                intra,
                            } => Row::Line {
                                old_no: None,
                                new_no: Some(*new_no),
                                kind: LineKind::Added,
                                text: text.clone().into(),
                                intra: intra.clone(),
                                syntax,
                            },
                            DiffRow::Removed {
                                old_no,
                                text,
                                intra,
                            } => Row::Line {
                                old_no: Some(*old_no),
                                new_no: None,
                                kind: LineKind::Removed,
                                text: text.clone().into(),
                                intra: intra.clone(),
                                syntax,
                            },
                        });
                    }
                }
                ViewMode::Split => {
                    let hrows = &hunk.rows;
                    let mut i = 0;
                    while i < hrows.len() {
                        match &hrows[i] {
                            DiffRow::Context {
                                old_no,
                                new_no,
                                text,
                            } => {
                                let text: SharedString = text.clone().into();
                                let syntax = syntax_spans[i].clone();
                                rows.push(Row::SplitLine {
                                    left: Some(Cell {
                                        no: *old_no,
                                        kind: LineKind::Context,
                                        text: text.clone(),
                                        intra: Vec::new(),
                                        syntax: syntax.clone(),
                                    }),
                                    right: Some(Cell {
                                        no: *new_no,
                                        kind: LineKind::Context,
                                        text,
                                        intra: Vec::new(),
                                        syntax,
                                    }),
                                });
                                i += 1;
                            }
                            DiffRow::Added {
                                new_no,
                                text,
                                intra,
                            } => {
                                rows.push(Row::SplitLine {
                                    left: None,
                                    right: Some(Cell {
                                        no: *new_no,
                                        kind: LineKind::Added,
                                        text: text.clone().into(),
                                        intra: intra.clone(),
                                        syntax: syntax_spans[i].clone(),
                                    }),
                                });
                                i += 1;
                            }
                            DiffRow::Removed { .. } => {
                                let start = i;
                                while i < hrows.len() && matches!(hrows[i], DiffRow::Removed { .. })
                                {
                                    i += 1;
                                }
                                let mid = i;
                                while i < hrows.len() && matches!(hrows[i], DiffRow::Added { .. }) {
                                    i += 1;
                                }
                                let (removed, added) = (mid - start, i - mid);
                                for pair in 0..removed.max(added) {
                                    let left =
                                        (pair < removed).then(|| match &hrows[start + pair] {
                                            DiffRow::Removed {
                                                old_no,
                                                text,
                                                intra,
                                            } => Cell {
                                                no: *old_no,
                                                kind: LineKind::Removed,
                                                text: text.clone().into(),
                                                intra: intra.clone(),
                                                syntax: syntax_spans[start + pair].clone(),
                                            },
                                            _ => unreachable!(),
                                        });
                                    let right = (pair < added).then(|| match &hrows[mid + pair] {
                                        DiffRow::Added {
                                            new_no,
                                            text,
                                            intra,
                                        } => Cell {
                                            no: *new_no,
                                            kind: LineKind::Added,
                                            text: text.clone().into(),
                                            intra: intra.clone(),
                                            syntax: syntax_spans[mid + pair].clone(),
                                        },
                                        _ => unreachable!(),
                                    });
                                    rows.push(Row::SplitLine { left, right });
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    (rows, file_rows, hunk_rows)
}

fn kind_style(
    kind: LineKind,
) -> (
    Option<gpui::Rgba>,
    Option<gpui::Rgba>,
    &'static str,
    gpui::Rgba,
) {
    match kind {
        LineKind::Context => (None, None, "", theme::overlay0()),
        LineKind::Added => (
            Some(theme::added_row_bg()),
            Some(theme::added_word_bg()),
            "+",
            theme::green(),
        ),
        LineKind::Removed => (
            Some(theme::removed_row_bg()),
            Some(theme::removed_word_bg()),
            "−",
            theme::red(),
        ),
    }
}

/// Overlay syntax color spans, intra word-diff background ranges, and the
/// selection background into one sorted, non-overlapping highlight list.
fn merge_highlights(
    syntax: &[(Range<usize>, syntax::Token)],
    intra: &[Range<usize>],
    word_bg: Option<gpui::Rgba>,
    selection: Option<Range<usize>>,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let mut bounds = Vec::with_capacity(2 * (syntax.len() + intra.len() + 1));
    for (range, _) in syntax {
        bounds.push(range.start);
        bounds.push(range.end);
    }
    for range in intra {
        bounds.push(range.start);
        bounds.push(range.end);
    }
    if let Some(sel) = &selection {
        bounds.push(sel.start);
        bounds.push(sel.end);
    }
    bounds.sort_unstable();
    bounds.dedup();

    let mut out: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    let (mut si, mut ii) = (0, 0);
    for seg in bounds.windows(2) {
        let (start, end) = (seg[0], seg[1]);
        while si < syntax.len() && syntax[si].0.end <= start {
            si += 1;
        }
        while ii < intra.len() && intra[ii].end <= start {
            ii += 1;
        }
        let token = (si < syntax.len() && syntax[si].0.start <= start).then(|| syntax[si].1);
        let in_intra = ii < intra.len() && intra[ii].start <= start;
        let in_sel = selection
            .as_ref()
            .is_some_and(|sel| sel.start <= start && start < sel.end);
        if token.is_none() && !in_intra && !in_sel {
            continue;
        }
        let mut style = token.map(theme::token_style).unwrap_or_default();
        if in_intra {
            style.background_color = word_bg.map(Into::into);
        }
        if in_sel {
            style.background_color = Some(theme::selection_bg().into());
        }
        match out.last_mut() {
            Some((prev, prev_style)) if prev.end == start && *prev_style == style => prev.end = end,
            _ => out.push((start..end, style)),
        }
    }
    out
}

/// Line text with syntax colors overlaid with word-level highlights and the
/// selection background, shared by unified rows and split cells.
fn line_content(
    text: &SharedString,
    syntax: &[(Range<usize>, syntax::Token)],
    intra: &[Range<usize>],
    word_bg: Option<gpui::Rgba>,
    selection: Option<Range<usize>>,
) -> gpui::AnyElement {
    let highlights = merge_highlights(syntax, intra, word_bg, selection);
    if highlights.is_empty() {
        div().child(text.clone()).into_any_element()
    } else {
        StyledText::new(text.clone())
            .with_highlights(highlights)
            .into_any_element()
    }
}

/// `split_x` is the horizontal text scroll shared by both split cells; the
/// cells themselves stay pinned to half the pane so both sides are visible.
fn render_row(row: &Row, selection: Option<(SelSide, Range<usize>)>, split_x: Pixels) -> gpui::AnyElement {
    let row_height = px(row_height());
    match row {
        Row::Spacer => div().h(row_height).into_any_element(),
        Row::FileHeader {
            path,
            old_path,
            status,
            additions,
            deletions,
        } => {
            let (status_label, status_color) = status_style(*status);
            let status: Hsla = status_color.into();
            let mut header = div()
                .h(row_height)
                .w_full()
                .flex()
                .items_center()
                .gap_3()
                .px_3()
                .bg(theme::mantle())
                .child(
                    Tag::custom(status.opacity(0.15), status, status.opacity(0.4))
                        .small()
                        .child(SharedString::from(status_label)),
                )
                .child(
                    div()
                        .text_color(theme::text())
                        .font_weight(gpui::FontWeight::BOLD)
                        .child(path.clone()),
                );
            if let Some(old_path) = old_path {
                header = header.child(
                    div()
                        .text_color(theme::overlay0())
                        .child(SharedString::from(format!("← {old_path}"))),
                );
            }
            header
                .child(div().flex_1())
                .child(
                    div()
                        .text_color(theme::green())
                        .child(SharedString::from(format!("+{additions}"))),
                )
                .child(
                    div()
                        .text_color(theme::red())
                        .child(SharedString::from(format!("−{deletions}"))),
                )
                .into_any_element()
        }
        Row::HunkHeader { label } => div()
            .h(row_height)
            .w_full()
            .flex()
            .items_center()
            .px_3()
            .bg(theme::crust())
            .text_color(theme::overlay0())
            .child(label.clone())
            .into_any_element(),
        Row::Binary => div()
            .h(row_height)
            .flex()
            .items_center()
            .px_3()
            .text_color(theme::overlay0())
            .child(SharedString::from("binary file changed"))
            .into_any_element(),
        Row::Line {
            old_no,
            new_no,
            kind,
            text,
            intra,
            syntax,
        } => {
            let (row_bg, word_bg, marker, marker_color) = kind_style(*kind);
            let number = |no: Option<u32>| {
                div()
                    .w(px(44.))
                    .flex_shrink_0()
                    .text_color(theme::overlay0())
                    .flex()
                    .justify_end()
                    .child(SharedString::from(
                        no.map(|no| no.to_string()).unwrap_or_default(),
                    ))
            };
            let mut line = div().h(row_height).flex().items_center();
            if let Some(bg) = row_bg {
                line = line.bg(bg);
            }
            line.child(number(*old_no))
                .child(number(*new_no))
                .child(
                    div()
                        .w(px(28.))
                        .flex_shrink_0()
                        .flex()
                        .justify_center()
                        .text_color(marker_color)
                        .child(SharedString::from(marker)),
                )
                .child(div().whitespace_nowrap().child(line_content(
                    text,
                    syntax,
                    intra,
                    word_bg,
                    selection.map(|(_, range)| range),
                )))
                .into_any_element()
        }
        Row::SplitLine { left, right } => {
            let (left_sel, right_sel) = match selection {
                Some((SelSide::Left, range)) => (Some(range), None),
                Some((SelSide::Right, range)) => (None, Some(range)),
                _ => (None, None),
            };
            let cell = |cell: &Option<Cell>, sel: Option<Range<usize>>| {
                let base = div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .h_full()
                    .flex()
                    .items_center();
                let Some(cell) = cell else {
                    return base.bg(theme::void_cell_bg());
                };
                let (row_bg, word_bg, marker, marker_color) = kind_style(cell.kind);
                let mut side = base;
                if let Some(bg) = row_bg {
                    side = side.bg(bg);
                }
                side.child(
                    div()
                        .w(px(44.))
                        .flex_shrink_0()
                        .text_color(theme::overlay0())
                        .flex()
                        .justify_end()
                        .child(SharedString::from(cell.no.to_string())),
                )
                .child(
                    div()
                        .w(px(28.))
                        .flex_shrink_0()
                        .flex()
                        .justify_center()
                        .text_color(marker_color)
                        .child(SharedString::from(marker)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .overflow_hidden()
                        .flex()
                        .items_center()
                        .child(
                            div()
                                .flex_shrink_0()
                                .relative()
                                .left(-split_x)
                                .whitespace_nowrap()
                                .child(line_content(
                                    &cell.text,
                                    &cell.syntax,
                                    &cell.intra,
                                    word_bg,
                                    sel,
                                )),
                        ),
                )
            };
            div()
                .w_full()
                .h(row_height)
                .flex()
                .child(cell(left, left_sel))
                .child(
                    div()
                        .w(px(6.))
                        .flex_shrink_0()
                        .h_full()
                        .bg(theme::crust())
                        .border_l_1()
                        .border_r_1()
                        .border_color(theme::surface0()),
                )
                .child(cell(right, right_sel))
                .into_any_element()
        }
    }
}

// --- Sidebar file tree ---------------------------------------------------

const TREE_ROW_HEIGHT: f32 = 28.0;

#[derive(Debug, PartialEq)]
struct TreeEntry {
    depth: usize,
    name: SharedString,
    kind: TreeEntryKind,
}

#[derive(Debug, PartialEq)]
enum TreeEntryKind {
    Dir { path: String },
    File { file_ix: usize },
}

fn build_tree(paths: &[&str]) -> Vec<TreeEntry> {
    #[derive(Default)]
    struct DirNode {
        dirs: std::collections::BTreeMap<String, DirNode>,
        files: Vec<(String, usize)>,
    }
    let mut root = DirNode::default();
    for (file_ix, path) in paths.iter().enumerate() {
        let (dirs, name) = match path.rsplit_once('/') {
            Some((dirs, name)) => (Some(dirs), name),
            None => (None, *path),
        };
        let mut node = &mut root;
        for part in dirs.into_iter().flat_map(|dirs| dirs.split('/')) {
            node = node.dirs.entry(part.to_string()).or_default();
        }
        node.files.push((name.to_string(), file_ix));
    }
    fn flatten(node: DirNode, prefix: &str, depth: usize, out: &mut Vec<TreeEntry>) {
        for (name, mut child) in node.dirs {
            let mut label = name;
            let mut path = if prefix.is_empty() {
                label.clone()
            } else {
                format!("{prefix}/{label}")
            };
            while child.files.is_empty() && child.dirs.len() == 1 {
                let (next_name, next) = child.dirs.into_iter().next().unwrap();
                label.push('/');
                label.push_str(&next_name);
                path.push('/');
                path.push_str(&next_name);
                child = next;
            }
            out.push(TreeEntry {
                depth,
                name: label.into(),
                kind: TreeEntryKind::Dir { path: path.clone() },
            });
            flatten(child, &path, depth + 1, out);
        }
        let mut files = node.files;
        files.sort();
        for (name, file_ix) in files {
            out.push(TreeEntry {
                depth,
                name: name.into(),
                kind: TreeEntryKind::File { file_ix },
            });
        }
    }
    let mut out = Vec::new();
    flatten(root, "", 0, &mut out);
    out
}

fn visible_entries(entries: &[TreeEntry], collapsed: &HashSet<String>) -> Vec<usize> {
    let mut out = Vec::with_capacity(entries.len());
    let mut hide_deeper_than: Option<usize> = None;
    for (ix, entry) in entries.iter().enumerate() {
        if let Some(depth) = hide_deeper_than {
            if entry.depth > depth {
                continue;
            }
            hide_deeper_than = None;
        }
        out.push(ix);
        if let TreeEntryKind::Dir { path } = &entry.kind {
            if collapsed.contains(path) {
                hide_deeper_than = Some(entry.depth);
            }
        }
    }
    out
}

fn fuzzy_file_matches(paths: &[&str], query: &str) -> Vec<usize> {
    let query = query.trim();
    if query.is_empty() {
        return (0..paths.len()).collect();
    }
    let matcher = SkimMatcherV2::default();
    let mut scored: Vec<(i64, usize)> = paths
        .iter()
        .enumerate()
        .filter_map(|(ix, path)| matcher.fuzzy_match(path, query).map(|score| (score, ix)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, ix)| ix).collect()
}

fn status_style(status: FileStatus) -> (&'static str, gpui::Rgba) {
    match status {
        FileStatus::Added => ("added", theme::green()),
        FileStatus::Deleted => ("deleted", theme::red()),
        FileStatus::Modified => ("modified", theme::blue()),
        FileStatus::Renamed => ("renamed", theme::mauve()),
        FileStatus::Binary => ("binary", theme::peach()),
    }
}

#[derive(Clone, Copy)]
enum TreeListRow {
    Entry(usize),
    FilteredFile(usize),
}

fn render_tree_row(
    row: TreeListRow,
    pos: usize,
    current: bool,
    cursor: bool,
    data: &ItemData,
    entity: &gpui::Entity<ReviewApp>,
) -> gpui::AnyElement {
    let stats = |file: &FileDiff| {
        div()
            .flex()
            .items_center()
            .gap_1()
            .flex_shrink_0()
            .text_size(px(12.))
            .child(
                div()
                    .text_color(Hsla::from(theme::green()).opacity(0.7))
                    .child(SharedString::from(format!("+{}", file.additions))),
            )
            .child(
                div()
                    .text_color(Hsla::from(theme::red()).opacity(0.7))
                    .child(SharedString::from(format!("−{}", file.deletions))),
            )
    };
    let entity = entity.clone();
    let base = div()
        .id(("tree-row", pos))
        .h(px(TREE_ROW_HEIGHT))
        .w_full()
        .flex()
        .items_center()
        .gap_1()
        .pr_2()
        .cursor_pointer()
        .when(current, |row| row.bg(theme::surface0()))
        .when(!current, |row| {
            row.hover(|style| style.bg(Hsla::from(theme::surface0()).opacity(0.5)))
        })
        .when(cursor, |row| row.bg(Hsla::from(theme::blue()).opacity(0.25)));
    match row {
        TreeListRow::Entry(entry_ix) => {
            let entry = &data.tree[entry_ix];
            let indent = px(8. + entry.depth as f32 * 12.);
            let base = base.pl(indent).on_click(move |_, window, cx| {
                entity.update(cx, |this, cx| this.tree_entry_clicked(entry_ix, window, cx));
            });
            match &entry.kind {
                TreeEntryKind::Dir { path } => {
                    let chevron = if data.collapsed.contains(path) {
                        "▸"
                    } else {
                        "▾"
                    };
                    base.child(
                        div()
                            .w(px(12.))
                            .flex_shrink_0()
                            .text_color(theme::overlay0())
                            .child(SharedString::from(chevron)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(theme::overlay0())
                            .child(entry.name.clone()),
                    )
                    .into_any_element()
                }
                TreeEntryKind::File { file_ix } => {
                    let file = &data.diff.files[*file_ix];
                    base.child(div().w(px(12.)).flex_shrink_0())
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(status_style(file.status).1)
                                .child(entry.name.clone()),
                        )
                        .child(stats(file))
                        .into_any_element()
                }
            }
        }
        TreeListRow::FilteredFile(file_ix) => {
            let file = &data.diff.files[file_ix];
            base.pl_2()
                .on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| this.jump_to_file(file_ix, window, cx));
                })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(status_style(file.status).1)
                        .child(SharedString::from(file.display_path().to_string())),
                )
                .child(stats(file))
                .into_any_element()
        }
    }
}

    fn render_exclude_tag(
        ix: usize,
        pattern: SharedString,
        entity: &gpui::Entity<ReviewApp>,
    ) -> gpui::AnyElement {
        let entity = entity.clone();
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(theme::surface0())
            .flex()
            .items_center()
            .gap_1()
            .text_size(px(12.))
            .max_w_full()
            .child(
                div()
                    .text_color(theme::text())
                    .truncate()
                    .child(pattern.clone()),
            )
            .child(
                div()
                    .id(("exclude-remove", ix))
                    .flex_shrink_0()
                    .text_color(theme::overlay0())
                    .cursor_pointer()
                    .hover(|s| s.text_color(theme::red()))
                    .child(SharedString::from("×"))
                    .on_click(move |_, _, cx| {
                        entity.update(cx, |this, cx| this.remove_exclude(ix, cx));
                    }),
            )
            .into_any_element()
    }

// --- Selection ------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SelSide {
    Unified,
    Left,
    Right,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct RowCol {
    row: usize,
    col: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Selection {
    side: SelSide,
    anchor: RowCol,
    head: RowCol,
}

impl Selection {
    fn ordered(&self) -> (RowCol, RowCol) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

fn row_side_text(row: &Row, side: SelSide) -> Option<&str> {
    match (row, side) {
        (Row::Line { text, .. }, SelSide::Unified) => Some(text.as_ref()),
        (Row::SplitLine { left, .. }, SelSide::Left) => left.as_ref().map(|c| c.text.as_ref()),
        (Row::SplitLine { right, .. }, SelSide::Right) => right.as_ref().map(|c| c.text.as_ref()),
        _ => None,
    }
}

fn char_to_byte(text: &str, col: usize) -> usize {
    text.char_indices()
        .nth(col)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len())
}

fn row_selection_range(sel: &Selection, row_ix: usize, row: &Row) -> Option<Range<usize>> {
    let (start, end) = sel.ordered();
    if row_ix < start.row || row_ix > end.row {
        return None;
    }
    let text = row_side_text(row, sel.side)?;
    let chars = text.chars().count();
    let start_col = if row_ix == start.row {
        start.col.min(chars)
    } else {
        0
    };
    let end_col = if row_ix == end.row {
        end.col.min(chars)
    } else {
        chars
    };
    if start_col > end_col {
        return None;
    }
    Some(char_to_byte(text, start_col)..char_to_byte(text, end_col))
}

fn selection_text(sel: &Selection, rows: &[Row]) -> String {
    let (start, end) = sel.ordered();
    let mut parts = Vec::new();
    for ix in start.row..=end.row.min(rows.len().saturating_sub(1)) {
        if let Some(range) = row_selection_range(sel, ix, &rows[ix]) {
            let text = row_side_text(&rows[ix], sel.side).unwrap_or_default();
            parts.push(&text[range]);
        }
    }
    parts.join("\n")
}

// --- Item data ------------------------------------------------------------

struct ItemData {
    src: git::LocalSource,
    diff: PrDiff,
    mode: ViewMode,
    rows: Vec<Row>,
    file_rows: Vec<usize>,
    hunk_rows: Vec<usize>,
    max_line_chars: usize,
    widest_row_ix: usize,
    split_scroll_x: f32,
    cursor: usize,
    scroll: UniformListScrollHandle,
    additions: u32,
    deletions: u32,
    selection: Option<Selection>,
    tree: Vec<TreeEntry>,
    collapsed: HashSet<String>,
    tree_scroll: UniformListScrollHandle,
    tree_last_file: Option<usize>,
}

impl ItemData {
    fn set_rows(&mut self, (rows, file_rows, hunk_rows): (Vec<Row>, Vec<usize>, Vec<usize>)) {
        let (widest_row_ix, max_line_chars) = widest_line(&rows);
        self.widest_row_ix = widest_row_ix;
        self.max_line_chars = max_line_chars;
        self.rows = rows;
        self.file_rows = file_rows;
        self.hunk_rows = hunk_rows;
    }

    /// The file being looked at: the cursor's file while the cursor is on
    /// screen (hunk jumps center their hunk, so the top row can still be the
    /// previous file's tail), else the file at the top of the viewport.
    fn viewed_file(&self) -> Option<usize> {
        let rh = row_height();
        let scroll = self.scroll.0.borrow();
        let (top, bottom) = match &scroll.deferred_scroll_to_item {
            Some(deferred) => (deferred.item_index, usize::MAX),
            None => {
                let top_px = f32::from(-scroll.base_handle.offset().y).max(0.);
                let height = f32::from(scroll.base_handle.bounds().size.height);
                ((top_px / rh) as usize, ((top_px + height) / rh) as usize)
            }
        };
        drop(scroll);
        let row = if (top..bottom).contains(&self.cursor) {
            self.cursor
        } else {
            top
        };
        self.file_rows.iter().rposition(|&ix| ix <= row)
    }

    /// The hunk the cursor is in, as (index into `hunk_rows`, first row,
    /// end row exclusive). None when the cursor sits on a file header or
    /// before the first hunk.
    fn current_hunk(&self) -> Option<(usize, usize, usize)> {
        let pos = self.hunk_rows.iter().rposition(|&ix| ix <= self.cursor)?;
        let start = self.hunk_rows[pos];
        let next_hunk = self.hunk_rows.get(pos + 1).copied();
        let next_file = self.file_rows.iter().copied().find(|&ix| ix > start);
        let end = [next_hunk, next_file]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(self.rows.len());
        (self.cursor < end).then_some((pos, start, end))
    }

    fn rebuild_tree(&mut self) {
        let paths: Vec<&str> = self.diff.files.iter().map(|f| f.display_path()).collect();
        let tree = build_tree(&paths);
        self.collapsed.retain(|path| {
            tree.iter()
                .any(|e| matches!(&e.kind, TreeEntryKind::Dir { path: p } if p == path))
        });
        self.tree = tree;
        self.tree_last_file = None;
    }
}

struct Loaded {
    src: git::LocalSource,
    diff: PrDiff,
    rows: Vec<Row>,
    file_rows: Vec<usize>,
    hunk_rows: Vec<usize>,
    mode: ViewMode,
    excluded: usize,
    patch_hash: u64,
}

/// Whether a changed path can affect the diff: worktree files, plus `HEAD` and
/// refs (commits, checkouts). The rest of `.git` and build/dependency dirs
/// churn constantly without changing what we show.
fn is_relevant_change(root: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    let mut parts = rel.components().map(|c| c.as_os_str());
    match parts.next() {
        None => false,
        Some(first) if first == ".git" => match parts.next() {
            Some(second) => second == "HEAD" || second == "refs",
            None => false,
        },
        Some(first) => std::iter::once(first)
            .chain(parts)
            .all(|part| part != "node_modules" && part != "target"),
    }
}

/// Patterns excluded on startup; each shows as a removable tag.
const DEFAULT_EXCLUDES: &[&str] = &["__generated__", "*.wasm", "*.glb", "*.png"];

/// Whether `path` is excluded by `pattern`. The glob is tried against every
/// run of whole path segments, so `__generated__` excludes any file under a
/// directory of that name, `*.snap` matches by basename, and
/// `*/__generated__/*` also covers a top-level `__generated__/`.
fn path_excluded(pattern: &str, path: &str) -> bool {
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

fn fetch_item(
    path: &Path,
    mode: ViewMode,
    exclude: &[String],
    query: &str,
) -> anyhow::Result<Loaded> {
    let src = git::resolve_local(path)?;
    let patch = git::diff_patch(&src)?;
    let patch_hash = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        patch.hash(&mut h);
        h.finish()
    };
    let mut diff = diff_core::parse_patch(&patch);
    let total = diff.files.len();
    diff.files
        .retain(|f| !exclude.iter().any(|pat| path_excluded(pat, f.display_path())));
    let excluded = total - diff.files.len();
    let (rows, file_rows, hunk_rows) = build_rows(&diff, mode, shown_files(&diff, query).as_ref());
    Ok(Loaded {
        src,
        diff,
        excluded,
        patch_hash,
        rows,
        file_rows,
        hunk_rows,
        mode,
    })
}

// --- Titlebar ------------------------------------------------------------

fn centered_message(text: SharedString, color: gpui::Rgba) -> gpui::AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_color(color)
        .child(text)
        .into_any_element()
}

fn app_title(detail: Option<String>) -> gpui::AnyElement {
    let mut title = div().flex().items_center().gap_2().flex_1().min_w_0();
    if let Some(detail) = detail {
        title = title.child(
            div()
                .text_color(theme::subtext())
                .truncate()
                .child(SharedString::from(detail)),
        );
    }
    title.into_any_element()
}

fn local_titlebar_content(src: &git::LocalSource, data: &ItemData) -> gpui::AnyElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .flex_1()
        .min_w_0()
        .child(
            div()
                .text_color(theme::green())
                .child(SharedString::from(format!("+{}", data.additions))),
        )
        .child(
            div()
                .text_color(theme::red())
                .child(SharedString::from(format!("−{}", data.deletions))),
        )
        .child(
            div()
                .font_weight(gpui::FontWeight::BOLD)
                .truncate()
                .child(SharedString::from(src.branch.clone())),
        )
        .child(
            div()
                .text_color(theme::overlay0())
                .child(SharedString::from(format!("vs {}", src.base_label))),
        )
        .when_some(data.current_hunk(), |bar, (pos, _, _)| {
            bar.child(div().text_color(theme::blue()).child(SharedString::from(format!(
                "hunk {}/{}",
                pos + 1,
                data.hunk_rows.len()
            ))))
        })
        .into_any_element()
}

// --- App ------------------------------------------------------------------

enum LoadState {
    Loading,
    Ready(Box<ItemData>),
    Failed(String),
}

struct ReviewApp {
    state: LoadState,
    reloading: bool,
    refresh_error: Option<SharedString>,
    sidebar_visible: bool,
    sidebar_width: f32,
    sidebar_resizing: bool,
    sidebar_resize_start: Option<(f32, f32)>,
    titlebar_dragging: bool,
    keybindings_visible: bool,
    tree_filter_input: gpui::Entity<InputState>,
    exclude_input: gpui::Entity<InputState>,
    exclude: Vec<String>,
    /// Excludes the last fetch was spawned with, to skip no-op refetches.
    applied_exclude: Vec<String>,
    exclude_debounce: Option<gpui::Task<()>>,
    excluded_count: usize,
    /// Hash of the patch behind the installed rows, plus the mode/excludes it
    /// was built with; a refetch that matches is dropped so watcher noise
    /// doesn't reset scroll or selection.
    installed_key: Option<(u64, ViewMode, Vec<String>)>,
    _watcher: Option<notify::RecommendedWatcher>,
    focus_handle: FocusHandle,
    /// Keyboard focus for the sidebar file tree, and its cursor (a position
    /// in `tree_list_rows`).
    tree_focus: FocusHandle,
    tree_cursor: usize,
    drag_anchor: Option<(SelSide, RowCol)>,
    char_width: Option<Pixels>,
    repo_path: PathBuf,
    _subscriptions: Vec<Subscription>,
}

impl ReviewApp {
    fn new(repo_path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let tree_filter_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("filter files…"));
        let exclude_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("exclude, e.g. __generated__ (enter to pin)")
        });
        let _subscriptions = vec![
            cx.subscribe_in(
                &tree_filter_input,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => this.tree_filter_confirm(window, cx),
                    InputEvent::Change => {
                        if let Some(data) = this.active_data() {
                            data.tree_scroll.scroll_to_item(0, ScrollStrategy::Top);
                        }
                        this.apply_file_filter(cx);
                    }
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &exclude_input,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => this.add_exclude(window, cx),
                    InputEvent::Change => this.exclude_draft_changed(cx),
                    _ => {}
                },
            ),
        ];
        let mut this = Self {
            state: LoadState::Loading,
            reloading: false,
            refresh_error: None,
            sidebar_visible: true,
            sidebar_width: 340.,
            sidebar_resizing: false,
            sidebar_resize_start: None,
            titlebar_dragging: false,
            keybindings_visible: false,
            tree_filter_input,
            exclude_input,
            exclude: DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect(),
            applied_exclude: Vec::new(),
            exclude_debounce: None,
            excluded_count: 0,
            installed_key: None,
            _watcher: None,
            focus_handle: cx.focus_handle(),
            tree_focus: cx.focus_handle(),
            tree_cursor: 0,
            drag_anchor: None,
            char_width: None,
            repo_path,
            _subscriptions,
        };
        this.spawn_fetch(ViewMode::Split, cx);
        this
    }

    fn spawn_fetch(&mut self, mode: ViewMode, cx: &mut Context<Self>) {
        let repo = self.repo_path.clone();
        let exclude = self.effective_exclude(cx);
        self.applied_exclude = exclude.clone();
        let query = self.tree_filter_input.read(cx).value().to_string();
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn({
                    let exclude = exclude.clone();
                    let query = query.clone();
                    async move { fetch_item(&repo, mode, &exclude, &query) }
                })
                .await;
            this.update(cx, |app, cx| {
                app.reloading = false;
                // Excludes changed mid-fetch (the refresh they asked for was
                // skipped while this one ran): fetch again with the new list.
                if app.effective_exclude(cx) != exclude {
                    app.reloading = matches!(app.state, LoadState::Ready(_));
                    app.spawn_fetch(mode, cx);
                    return;
                }
                match fetched {
                    Ok(loaded) => {
                        let key = Some((loaded.patch_hash, loaded.mode, exclude));
                        let unchanged = key == app.installed_key
                            && matches!(&app.state, LoadState::Ready(d) if d.mode == loaded.mode);
                        if !unchanged {
                            app.installed_key = key;
                            app.install(loaded, cx);
                            // The file filter changed while fetching.
                            if app.tree_filter_input.read(cx).value().to_string() != query {
                                app.apply_file_filter(cx);
                            }
                        }
                    }
                    Err(err) => {
                        let msg = format!("{err:#}");
                        match &app.state {
                            LoadState::Ready(_) => app.refresh_error = Some(msg.into()),
                            _ => app.state = LoadState::Failed(msg),
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn install(&mut self, loaded: Loaded, cx: &mut Context<Self>) {
        let Loaded {
            src,
            diff,
            rows,
            file_rows,
            hunk_rows,
            mode,
            excluded,
            patch_hash: _,
        } = loaded;
        if self._watcher.is_none() {
            self.watch_repo(src.repo_root.clone(), cx);
        }
        self.excluded_count = excluded;
        let (additions, deletions) = diff
            .files
            .iter()
            .fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions));
        self.refresh_error = None;
        match &mut self.state {
            LoadState::Ready(data) => {
                data.src = src;
                data.diff = diff;
                data.additions = additions;
                data.deletions = deletions;
                data.set_rows((rows, file_rows, hunk_rows));
                data.cursor = data.cursor.min(data.rows.len().saturating_sub(1));
                data.selection = None;
                data.rebuild_tree();
            }
            _ => {
                let (widest_row_ix, max_line_chars) = widest_line(&rows);
                let mut data = Box::new(ItemData {
                    src,
                    diff,
                    mode,
                    rows,
                    file_rows,
                    hunk_rows,
                    max_line_chars,
                    widest_row_ix,
                    split_scroll_x: 0.,
                    cursor: 0,
                    scroll: UniformListScrollHandle::new(),
                    additions,
                    deletions,
                    selection: None,
                    tree: Vec::new(),
                    collapsed: HashSet::new(),
                    tree_scroll: UniformListScrollHandle::new(),
                    tree_last_file: None,
                });
                data.rebuild_tree();
                self.state = LoadState::Ready(data);
            }
        }
    }

    fn active_data(&self) -> Option<&ItemData> {
        match &self.state {
            LoadState::Ready(data) => Some(data),
            _ => None,
        }
    }

    fn active_data_mut(&mut self) -> Option<&mut ItemData> {
        match &mut self.state {
            LoadState::Ready(data) => Some(data),
            _ => None,
        }
    }

    fn zoom(&mut self, delta: f32, reset: bool, cx: &mut Context<Self>) {
        let old_rh = row_height();
        let next = if reset {
            DEFAULT_TEXT_SIZE
        } else {
            (text_size() + delta).clamp(MIN_TEXT_SIZE, MAX_TEXT_SIZE)
        };
        if next == text_size() {
            return;
        }
        FONT_PX.store(next as u32, Ordering::Relaxed);
        let new_rh = row_height();
        self.char_width = None;
        if let LoadState::Ready(data) = &mut self.state {
            let offset = data.scroll.0.borrow().base_handle.offset();
            let top_row = (-f32::from(offset.y) / old_rh).max(0.);
            data.scroll
                .0
                .borrow()
                .base_handle
                .set_offset(point(offset.x, px(-(top_row * new_rh))));
        }
        cx.notify();
    }

    fn char_width(&mut self, window: &Window) -> Pixels {
        *self.char_width.get_or_insert_with(|| {
            let text_system = window.text_system();
            let font_id = text_system.resolve_font(&font(MONO));
            text_system
                .em_advance(font_id, px(text_size()))
                .unwrap_or(px(text_size() * 0.6))
        })
    }

    /// Scroll split-view text horizontally, clamped so the longest line's end
    /// can just reach the right edge of its (half-pane) cell.
    fn scroll_split_x(&mut self, delta: f32, window: &Window) -> bool {
        let char_w = f32::from(self.char_width(window)).max(1.);
        let Some(data) = self.active_data_mut() else {
            return false;
        };
        let pane_w = f32::from(data.scroll.0.borrow().base_handle.bounds().size.width);
        let text_w = (pane_w - SPLIT_DIVIDER) / 2. - SPLIT_GUTTER;
        let max = ((data.max_line_chars as f32) * char_w - text_w + char_w).max(0.);
        let x = (data.split_scroll_x - delta).clamp(0., max);
        let changed = x != data.split_scroll_x;
        data.split_scroll_x = x;
        changed
    }

    fn pane_hit(
        &self,
        position: Point<Pixels>,
        char_width: Pixels,
        locked: Option<SelSide>,
    ) -> Option<(SelSide, RowCol)> {
        let (side, row, text_x) = self.pane_text_hit(position, locked)?;
        let col = (f32::from(text_x) / f32::from(char_width)).round().max(0.) as usize;
        Some((side, RowCol { row, col }))
    }

    fn pane_text_hit(
        &self,
        position: Point<Pixels>,
        locked: Option<SelSide>,
    ) -> Option<(SelSide, usize, Pixels)> {
        let data = self.active_data()?;
        if data.rows.is_empty() {
            return None;
        }
        let (bounds, offset) = {
            let state = data.scroll.0.borrow();
            (state.base_handle.bounds(), state.base_handle.offset())
        };
        let y = f32::from(position.y - bounds.top() - offset.y);
        let row = ((y / row_height()).floor().max(0.) as usize).min(data.rows.len() - 1);
        let rel_x = f32::from(position.x - bounds.left());
        let (side, text_x) = match data.mode {
            ViewMode::Unified => (
                SelSide::Unified,
                rel_x - f32::from(offset.x) - UNIFIED_GUTTER,
            ),
            ViewMode::Split => {
                let half = (f32::from(bounds.size.width) - SPLIT_DIVIDER) / 2.;
                let side = locked.unwrap_or(if rel_x < half + SPLIT_DIVIDER / 2. {
                    SelSide::Left
                } else {
                    SelSide::Right
                });
                let cell_x = match side {
                    SelSide::Right => rel_x - half - SPLIT_DIVIDER,
                    _ => rel_x,
                };
                (side, cell_x - SPLIT_GUTTER + data.split_scroll_x)
            }
        };
        Some((side, row, px(text_x)))
    }

    /// Refresh when files in the repo change. Events are drained on a short
    /// tick so a burst of saves (formatters, `git checkout`) is one refetch.
    fn watch_repo(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        use notify::Watcher;
        let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();
        let watch_root = root.clone();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                if matches!(event.kind, notify::EventKind::Access(_)) {
                    return;
                }
                for path in event.paths {
                    if is_relevant_change(&watch_root, &path) {
                        tx.send(path).ok();
                    }
                }
            }
        });
        let mut watcher = match watcher {
            Ok(w) => w,
            Err(err) => {
                self.refresh_error = Some(format!("file watching unavailable: {err}").into());
                return;
            }
        };
        if let Err(err) = watcher.watch(&root, notify::RecursiveMode::Recursive) {
            self.refresh_error = Some(format!("file watching unavailable: {err}").into());
            return;
        }
        self._watcher = Some(watcher);
        cx.spawn(async move |this, cx| {
            let mut dirty = false;
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(300))
                    .await;
                let mut paths: Vec<PathBuf> = rx.try_iter().collect();
                paths.sort();
                paths.dedup();
                if !paths.is_empty() && !dirty {
                    // Build output (dist/, caches) is usually gitignored and
                    // can't change the diff; `.git` paths are never "ignored".
                    let root = root.clone();
                    dirty = cx
                        .background_spawn(async move {
                            paths.iter().any(|p| p.starts_with(root.join(".git")))
                                || git::any_unignored(&root, &paths)
                        })
                        .await;
                }
                if !dirty {
                    continue;
                }
                let Ok(refreshed) = this.update(cx, |app, cx| {
                    if app.reloading || matches!(app.state, LoadState::Loading) {
                        return false;
                    }
                    app.refresh(cx);
                    true
                }) else {
                    break;
                };
                if refreshed {
                    dirty = false;
                }
            }
        })
        .detach();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.reloading || matches!(self.state, LoadState::Loading) {
            return;
        }
        let mode = match &self.state {
            LoadState::Ready(data) => data.mode,
            _ => ViewMode::Split,
        };
        match self.state {
            LoadState::Failed(_) => self.state = LoadState::Loading,
            _ => self.reloading = true,
        }
        self.refresh_error = None;
        self.spawn_fetch(mode, cx);
        cx.notify();
    }

    /// Rebuild the diff rows so the pane shows only the files matching the
    /// "filter files…" query, like the sidebar does.
    fn apply_file_filter(&mut self, cx: &mut Context<Self>) {
        let query = self.tree_filter_input.read(cx).value().to_string();
        if let Some(data) = self.active_data_mut() {
            data.set_rows(build_rows(&data.diff, data.mode, shown_files(&data.diff, &query).as_ref()));
            data.selection = None;
        }
        self.jump(0, cx);
    }

    fn toggle_view(&mut self, cx: &mut Context<Self>) {
        let query = self.tree_filter_input.read(cx).value().to_string();
        let Some(data) = self.active_data_mut() else {
            return;
        };
        let file_pos = data.file_rows.iter().rposition(|&ix| ix <= data.cursor);
        data.selection = None;
        data.mode = match data.mode {
            ViewMode::Unified => ViewMode::Split,
            ViewMode::Split => ViewMode::Unified,
        };
        data.set_rows(build_rows(&data.diff, data.mode, shown_files(&data.diff, &query).as_ref()));
        let target = file_pos
            .and_then(|pos| data.file_rows.get(pos).copied())
            .unwrap_or(0);
        self.jump(target, cx);
    }

    /// Move the cursor to `ix`. A hunk that fits on screen is centered
    /// vertically as a block; anything else (file headers, tall hunks) is
    /// aligned to the top so its start is visible.
    fn jump(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(data) = self.active_data_mut() {
            data.cursor = ix;
            let rh = row_height();
            let handle = data.scroll.0.borrow().base_handle.clone();
            let viewport = f32::from(handle.bounds().size.height);
            match data.current_hunk() {
                Some((_, start, end))
                    if viewport > 0. && (end - start) as f32 * rh < viewport =>
                {
                    let block = (end - start) as f32 * rh;
                    let max = (data.rows.len() as f32 * rh - viewport).max(0.);
                    let top = (start as f32 * rh - (viewport - block) / 2.).clamp(0., max);
                    handle.set_offset(point(handle.offset().x, px(-top)));
                }
                _ => data.scroll.scroll_to_item_strict(ix, ScrollStrategy::Top),
            }
        }
        cx.notify();
    }

    /// Arrow-key stepping: while the current hunk runs past the screen edge
    /// in the direction of travel, scroll by a page (keeping a few rows of
    /// overlap) until its end is visible; only then move to the next or
    /// previous hunk.
    fn step_hunk(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(data) = self.active_data() else {
            return;
        };
        if let Some((_, start, end)) = data.current_hunk() {
            let rh = row_height();
            let handle = data.scroll.0.borrow().base_handle.clone();
            let viewport = f32::from(handle.bounds().size.height);
            let top = -f32::from(handle.offset().y);
            let page = (viewport - 3. * rh).max(rh);
            let (start_y, end_y) = (start as f32 * rh, end as f32 * rh);
            let new_top = if viewport <= 0. {
                None
            } else if forward && end_y > top + viewport + 0.5 {
                Some((top + page).min(end_y - viewport))
            } else if !forward && start_y < top - 0.5 {
                Some((top - page).max(start_y))
            } else {
                None
            };
            if let Some(new_top) = new_top {
                handle.set_offset(point(handle.offset().x, px(-new_top.max(0.))));
                cx.notify();
                return;
            }
        }
        let targets = data.hunk_rows.clone();
        if forward {
            self.jump_next(&targets, cx)
        } else {
            self.jump_prev(&targets, cx)
        }
    }

    fn jump_next(&mut self, targets: &[usize], cx: &mut Context<Self>) {
        let Some(cursor) = self.active_data().map(|data| data.cursor) else {
            return;
        };
        let len = self.active_data().map_or(0, |data| data.rows.len());
        if let Some(&ix) = targets.iter().find(|&&ix| ix > cursor && ix < len) {
            self.jump(ix, cx);
        }
    }

    fn jump_prev(&mut self, targets: &[usize], cx: &mut Context<Self>) {
        let Some(cursor) = self.active_data().map(|data| data.cursor) else {
            return;
        };
        if let Some(&ix) = targets.iter().rev().find(|&&ix| ix < cursor) {
            self.jump(ix, cx);
        }
    }

    fn jump_to_file(&mut self, file_ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(&row) = self
            .active_data()
            .and_then(|data| data.file_rows.get(file_ix))
        else {
            return;
        };
        window.focus(&self.focus_handle);
        self.jump(row, cx);
    }

    fn tree_entry_clicked(&mut self, entry_ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(data) = self.active_data_mut() else {
            return;
        };
        match data.tree.get(entry_ix).map(|entry| &entry.kind) {
            Some(TreeEntryKind::Dir { path }) => {
                let path = path.clone();
                if !data.collapsed.remove(&path) {
                    data.collapsed.insert(path);
                }
                cx.notify();
            }
            Some(&TreeEntryKind::File { file_ix }) => self.jump_to_file(file_ix, window, cx),
            None => {}
        }
    }

    fn tree_filter_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.tree_filter_input.read(cx).value().trim().to_string();
        let Some(data) = self.active_data() else {
            return;
        };
        if query.is_empty() {
            return;
        }
        let paths: Vec<&str> = data.diff.files.iter().map(|f| f.display_path()).collect();
        if let Some(file_ix) = fuzzy_file_matches(&paths, &query).into_iter().next() {
            self.jump_to_file(file_ix, window, cx);
        }
    }

    fn add_exclude(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pattern = self.exclude_input.read(cx).value().trim().to_string();
        if pattern.is_empty() {
            return;
        }
        self.exclude_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        if !self.exclude.contains(&pattern) {
            self.exclude.push(pattern);
        }
        if self.effective_exclude(cx) != self.applied_exclude {
            self.refresh(cx);
        }
    }

    /// Pinned excludes plus whatever is typed in the exclude box, so a
    /// pattern takes effect while typing, before Enter pins it as a tag.
    fn effective_exclude(&self, cx: &App) -> Vec<String> {
        let mut exclude = self.exclude.clone();
        let draft = self.exclude_input.read(cx).value().trim().to_string();
        if !draft.is_empty() && !exclude.contains(&draft) {
            exclude.push(draft);
        }
        exclude
    }

    fn exclude_draft_changed(&mut self, cx: &mut Context<Self>) {
        self.exclude_debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(300))
                .await;
            this.update(cx, |this, cx| {
                if this.effective_exclude(cx) != this.applied_exclude {
                    this.refresh(cx);
                }
            })
            .ok();
        }));
    }

    fn remove_exclude(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.exclude.remove(ix);
        self.refresh(cx);
    }

    fn render_titlebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let content: gpui::AnyElement = match &self.state {
            LoadState::Ready(data) => local_titlebar_content(&data.src, data),
            LoadState::Loading => app_title(Some("loading…".to_string())),
            LoadState::Failed(_) => app_title(Some("failed".to_string())),
        };
        let note: Option<SharedString> = if self.reloading {
            Some("reloading…".into())
        } else {
            self.refresh_error
                .as_ref()
                .map(|err| SharedString::from(format!("refresh failed: {err}")))
        };
        div()
            .flex_shrink_0()
            .h(px(34.))
            .flex()
            .items_center()
            .justify_between()
            .pl(px(12.))
            .border_b_1()
            .border_color(theme::surface0())
            .bg(theme::mantle())
            .text_size(px(13.))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _: &MouseDownEvent, _, cx| {
                this.titlebar_dragging = true;
                cx.stop_propagation();
            }))
            .on_mouse_move(cx.listener(|this, _: &MouseMoveEvent, window, _| {
                if this.titlebar_dragging {
                    this.titlebar_dragging = false;
                    window.start_window_move();
                }
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _: &MouseUpEvent, _, _| {
                this.titlebar_dragging = false;
            }))
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _: &MouseUpEvent, _, _| {
                this.titlebar_dragging = false;
            }))
            .child(content)
            .when_some(note, |bar, note| {
                bar.child(
                    div()
                        .max_w(px(280.))
                        .truncate()
                        .text_color(theme::overlay0())
                        .pr_3()
                        .child(note),
                )
            })
    }

    /// The sidebar's rows: the collapsible tree, or a flat list of matches
    /// while the file filter has a query.
    fn tree_list_rows(&self, cx: &App) -> Vec<TreeListRow> {
        let query = self.tree_filter_input.read(cx).value().trim().to_string();
        let Some(data) = self.active_data() else {
            return Vec::new();
        };
        if query.is_empty() {
            visible_entries(&data.tree, &data.collapsed)
                .into_iter()
                .map(TreeListRow::Entry)
                .collect()
        } else {
            let paths: Vec<&str> = data.diff.files.iter().map(|f| f.display_path()).collect();
            fuzzy_file_matches(&paths, &query)
                .into_iter()
                .map(TreeListRow::FilteredFile)
                .collect()
        }
    }

    /// The file a sidebar row stands for (None for directories).
    fn tree_row_file(&self, row: TreeListRow) -> Option<usize> {
        match row {
            TreeListRow::FilteredFile(file_ix) => Some(file_ix),
            TreeListRow::Entry(ix) => match &self.active_data()?.tree.get(ix)?.kind {
                &TreeEntryKind::File { file_ix } => Some(file_ix),
                TreeEntryKind::Dir { .. } => None,
            },
        }
    }

    /// Focus the file tree with its cursor on the file shown in the diff.
    fn focus_tree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_visible = true;
        let rows = self.tree_list_rows(cx);
        let current = self.active_data().and_then(|data| data.viewed_file());
        if let Some(pos) = current.and_then(|file| {
            rows.iter().position(|&row| self.tree_row_file(row) == Some(file))
        }) {
            self.tree_cursor = pos;
        }
        self.tree_cursor = self.tree_cursor.min(rows.len().saturating_sub(1));
        window.focus(&self.tree_focus);
        cx.notify();
    }

    /// Move the tree cursor; landing on a file shows it in the diff while
    /// focus stays in the tree.
    fn tree_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        let rows = self.tree_list_rows(cx);
        if rows.is_empty() {
            return;
        }
        let pos = (self.tree_cursor as isize + delta).clamp(0, rows.len() as isize - 1) as usize;
        self.tree_cursor = pos;
        if let Some(data) = self.active_data() {
            data.tree_scroll.scroll_to_item(pos, ScrollStrategy::Center);
        }
        // Land on the file's first hunk so it becomes the current hunk and
        // ↑/↓ in the diff continue from there; header-only files (binary,
        // pure renames) land on the header.
        let row = self.tree_row_file(rows[pos]).and_then(|file| {
            let data = self.active_data()?;
            let header = *data.file_rows.get(file)?;
            let next_file = data
                .file_rows
                .iter()
                .copied()
                .find(|&ix| ix > header)
                .unwrap_or(data.rows.len());
            let first_hunk = data
                .hunk_rows
                .iter()
                .copied()
                .find(|&ix| ix > header && ix < next_file);
            Some(first_hunk.unwrap_or(header))
        });
        match row {
            Some(row) => self.jump(row, cx),
            None => cx.notify(),
        }
    }

    /// Enter on a file returns focus to the diff; on a directory it folds or
    /// unfolds it. Left/right (`fold` = Some) fold/unfold directories only.
    fn tree_activate(&mut self, fold: Option<bool>, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.tree_list_rows(cx);
        let Some(&row) = rows.get(self.tree_cursor) else {
            return;
        };
        if let TreeListRow::Entry(entry_ix) = row {
            if let Some(data) = self.active_data_mut() {
                if let Some(TreeEntryKind::Dir { path }) =
                    data.tree.get(entry_ix).map(|entry| &entry.kind)
                {
                    let path = path.clone();
                    let collapse = fold.unwrap_or(!data.collapsed.contains(&path));
                    if collapse {
                        data.collapsed.insert(path);
                    } else {
                        data.collapsed.remove(&path);
                    }
                    cx.notify();
                    return;
                }
            }
        }
        if fold.is_none() && self.tree_row_file(row).is_some() {
            window.focus(&self.focus_handle);
            cx.notify();
        }
    }

    fn render_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.tree_filter_input.read(cx).value().trim().to_string();
        let tree_rows = self.tree_list_rows(cx);
        let tree_focused = self.tree_focus.is_focused(window);
        let tree_cursor = self.tree_cursor;
        let mut current_file = None;
        let mut current_row = None;
        if let Some(data) = self.active_data() {
            current_file = data.viewed_file();
            current_row = current_file.and_then(|file| {
                tree_rows.iter().position(|row| match row {
                    TreeListRow::Entry(ix) => matches!(
                        &data.tree[*ix].kind,
                        TreeEntryKind::File { file_ix } if *file_ix == file
                    ),
                    TreeListRow::FilteredFile(file_ix) => *file_ix == file,
                })
            });
        }
        if let Some(file) = current_file {
            if let Some(data) = self.active_data_mut() {
                if data.tree_last_file != Some(file) {
                    data.tree_last_file = Some(file);
                    if let Some(pos) = current_row {
                        data.tree_scroll.scroll_to_item(pos, ScrollStrategy::Center);
                    }
                }
            }
        }
        let tree_scroll = self.active_data().map(|data| data.tree_scroll.clone());
        let entity = cx.entity();
        let tree_list: gpui::AnyElement = match tree_scroll {
            Some(scroll) if !tree_rows.is_empty() => {
                let entity = entity.clone();
                uniform_list("file-tree", tree_rows.len(), move |range, _window, cx| {
                    let this = entity.read(cx);
                    let Some(data) = this.active_data() else {
                        return Vec::new();
                    };
                    range
                        .filter_map(|pos| tree_rows.get(pos).map(|row| (pos, *row)))
                        .map(|(pos, row)| {
                            render_tree_row(
                                row,
                                pos,
                                current_row == Some(pos),
                                tree_focused && pos == tree_cursor,
                                data,
                                &entity,
                            )
                        })
                        .collect()
                })
                .track_scroll(scroll)
                .h_full()
                .into_any_element()
            }
            Some(_) if !query.is_empty() => div()
                .px_3()
                .py_2()
                .text_color(theme::overlay0())
                .child(SharedString::from("no matching files"))
                .into_any_element(),
            _ => div().into_any_element(),
        };

        div()
            .w(px(self.sidebar_width))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(theme::mantle())
            .border_r_1()
            .border_color(theme::surface0())
            .text_size(px(14.))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    // Escape in the sidebar hands focus back to the diff; the
                    // filter stays applied (it also filters the diff pane).
                    .on_action(cx.listener(|this, _: &InputEscape, window, cx| {
                        window.focus(&this.focus_handle);
                        cx.notify();
                    }))
                    .child(
                        div()
                            .p_2()
                            .child(Input::new(&self.tree_filter_input).small()),
                    )
                    .when(!self.exclude.is_empty(), |col| {
                        col.child(
                            div()
                                .px_2()
                                .pb_1()
                                .flex()
                                .flex_wrap()
                                .gap_1()
                                .children(
                                    self.exclude
                                        .iter()
                                        .cloned()
                                        .enumerate()
                                        .map(|(ix, pat)| {
                                            render_exclude_tag(ix, SharedString::from(pat), &entity)
                                        }),
                                ),
                        )
                    })
                    .child(
                        div()
                            .px_2()
                            .pb_1()
                            .child(Input::new(&self.exclude_input).small()),
                    )
                    .when(self.excluded_count > 0, |col| {
                        let n = self.excluded_count;
                        col.child(
                            div()
                                .px_2()
                                .pb_1()
                                .text_size(px(12.))
                                .text_color(theme::overlay0())
                                .child(SharedString::from(format!(
                                    "{n} file{} excluded",
                                    if n == 1 { "" } else { "s" }
                                ))),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .key_context("FileTree")
                            .track_focus(&self.tree_focus)
                            .on_action(cx.listener(|this, _: &TreeUp, _, cx| this.tree_move(-1, cx)))
                            .on_action(cx.listener(|this, _: &TreeDown, _, cx| this.tree_move(1, cx)))
                            .on_action(cx.listener(|this, _: &TreeOpen, window, cx| {
                                this.tree_activate(None, window, cx)
                            }))
                            .on_action(cx.listener(|this, _: &TreeCollapse, window, cx| {
                                this.tree_activate(Some(true), window, cx)
                            }))
                            .on_action(cx.listener(|this, _: &TreeExpand, window, cx| {
                                this.tree_activate(Some(false), window, cx)
                            }))
                            .on_action(cx.listener(|this, _: &FocusDiff, window, cx| {
                                window.focus(&this.focus_handle);
                                cx.notify();
                            }))
                            .child(tree_list),
                    ),
            )
    }

    fn render_sidebar_resizer(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .w(px(4.))
            .flex_shrink_0()
            .h_full()
            .cursor_col_resize()
            .bg(theme::surface0())
            .on_mouse_down(MouseButton::Left, cx.listener(|this, event: &MouseDownEvent, _, cx| {
                this.sidebar_resizing = true;
                this.sidebar_resize_start = Some((f32::from(event.position.x), this.sidebar_width));
                cx.stop_propagation();
            }))
            .into_any_element()
    }

    fn render_keybindings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        const BINDINGS: &[(&str, &str)] = &[
            ("]", "next file"),
            ("[", "previous file"),
            ("n", "next hunk"),
            ("p", "previous hunk"),
            ("down", "scroll hunk / next hunk"),
            ("up", "scroll hunk / previous hunk"),
            ("v", "unified / split"),
            ("/", "filter files"),
            ("ctrl-f", "filter files"),
            ("escape", "switch file tree / diff"),
            ("home", "top"),
            ("end", "bottom"),
            ("ctrl-b", "toggle sidebar"),
            ("r", "refresh"),
            ("ctrl-=", "bigger font"),
            ("ctrl--", "smaller font"),
            ("ctrl-0", "reset font"),
            ("ctrl-c", "copy selection"),
            ("ctrl-k", "keybindings"),
            ("ctrl-q", "quit"),
        ];
        div()
            .absolute()
            .size_full()
            .bg(Hsla::from(theme::crust()).opacity(0.8))
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _: &MouseDownEvent, _, cx| {
                this.keybindings_visible = false;
                cx.notify();
            }))
            .child(
                div()
                    .id("keybindings-panel")
                    .w(px(460.))
                    .max_h(px(560.))
                    .overflow_y_scroll()
                    .bg(theme::mantle())
                    .border_1()
                    .border_color(theme::surface0())
                    .rounded_lg()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .children(BINDINGS.iter().map(|(key, desc)| {
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .child(Kbd::new(Keystroke::parse(key).unwrap()))
                            .child(
                                div()
                                    .text_color(theme::overlay0())
                                    .child(SharedString::from(*desc)),
                            )
                    })),
            )
            .into_any_element()
    }

}

impl Render for ReviewApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let pane: gpui::AnyElement = match &self.state {
            LoadState::Loading => centered_message("loading…".into(), theme::overlay0()),
            LoadState::Failed(msg) => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p_8()
                .child(
                    div()
                        .max_w(px(720.))
                        .text_color(theme::red())
                        .child(SharedString::from(msg.clone())),
                )
                .into_any_element(),
            LoadState::Ready(data) => {
                let rows_len = data.rows.len();
                let widest_row_ix = data.widest_row_ix;
                let scroll = data.scroll.clone();
                let split_x = px(data.split_scroll_x);
                let is_split = data.mode == ViewMode::Split;
                div()
                    .size_full()
                    .relative()
                    .flex()
                    .font_family(MONO)
                    .text_size(px(text_size()))
                    .line_height(px(row_height()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            window.focus(&this.focus_handle);
                            let char_width = this.char_width(window);
                            this.drag_anchor = this.pane_hit(event.position, char_width, None);
                            if let Some(data) = this.active_data_mut() {
                                if data.selection.take().is_some() {
                                    cx.notify();
                                }
                            }
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                        if !event.dragging() {
                            return;
                        }
                        let Some((side, anchor)) = this.drag_anchor else {
                            return;
                        };
                        let char_width = this.char_width(window);
                        let Some((_, head)) = this.pane_hit(event.position, char_width, Some(side))
                        else {
                            return;
                        };
                        let selection =
                            (head != anchor).then_some(Selection { side, anchor, head });
                        if let Some(data) = this.active_data_mut() {
                            if data.selection != selection {
                                data.selection = selection;
                                cx.notify();
                            }
                        }
                    }))
                    .when(is_split, |pane| {
                        pane.on_scroll_wheel(cx.listener(
                            |this, event: &ScrollWheelEvent, window, cx| {
                                let delta = event.delta.pixel_delta(px(row_height()));
                                let dx = if delta.x != px(0.) {
                                    delta.x
                                } else if event.modifiers.shift {
                                    delta.y
                                } else {
                                    return;
                                };
                                if this.scroll_split_x(f32::from(dx), window) {
                                    cx.notify();
                                }
                            },
                        ))
                    })
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseUpEvent, _, _| {
                            this.drag_anchor = None;
                        }),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseUpEvent, _, _| {
                            this.drag_anchor = None;
                        }),
                    )
                    .child(
                        uniform_list("diff", rows_len, move |range, _window, cx| {
                            let this = entity.read(cx);
                            match this.active_data() {
                                Some(data) => {
                                    let sel = data.selection;
                                    let hunk = data.current_hunk().map(|(_, start, end)| start..end);
                                    range
                                        .filter_map(|ix| data.rows.get(ix).map(|row| (ix, row)))
                                        .map(|(ix, row)| {
                                            let row_sel = sel.and_then(|sel| {
                                                row_selection_range(&sel, ix, row)
                                                    .filter(|range| !range.is_empty())
                                                    .map(|range| (sel.side, range))
                                            });
                                            let el = render_row(row, row_sel, split_x);
                                            if hunk.as_ref().is_some_and(|h| h.contains(&ix)) {
                                                div()
                                                    .relative()
                                                    .child(el)
                                                    .child(
                                                        div()
                                                            .absolute()
                                                            .left_0()
                                                            .top_0()
                                                            .bottom_0()
                                                            .w(px(3.))
                                                            .bg(theme::blue()),
                                                    )
                                                    .into_any_element()
                                            } else {
                                                el
                                            }
                                        })
                                        .collect()
                                }
                                None => Vec::new(),
                            }
                        })
                        .track_scroll(scroll)
                        .with_horizontal_sizing_behavior(
                            ListHorizontalSizingBehavior::Unconstrained,
                        )
                        .with_width_from_item(Some(widest_row_ix))
                        .h_full()
                        .flex_1()
                        .min_w_0(),
                    )
                    .child(Scrollbar::new(&data.scroll))
                    .into_any_element()
            }
        };
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(theme::base())
            .text_color(theme::text())
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                if !event.dragging() || !this.sidebar_resizing {
                    return;
                }
                let Some((start_x, start_width)) = this.sidebar_resize_start else {
                    return;
                };
                let delta = f32::from(event.position.x) - start_x;
                this.sidebar_width = (start_width + delta).clamp(180., 640.);
                cx.notify();
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _: &MouseUpEvent, _, _| {
                this.sidebar_resizing = false;
                this.sidebar_resize_start = None;
            }))
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _: &MouseUpEvent, _, _| {
                this.sidebar_resizing = false;
                this.sidebar_resize_start = None;
            }))
            .on_action(cx.listener(|this, _: &NextFile, _, cx| {
                let targets = this
                    .active_data()
                    .map(|d| d.file_rows.clone())
                    .unwrap_or_default();
                this.jump_next(&targets, cx)
            }))
            .on_action(cx.listener(|this, _: &PrevFile, _, cx| {
                let targets = this
                    .active_data()
                    .map(|d| d.file_rows.clone())
                    .unwrap_or_default();
                this.jump_prev(&targets, cx)
            }))
            .on_action(cx.listener(|this, _: &NextHunk, _, cx| {
                let targets = this
                    .active_data()
                    .map(|d| d.hunk_rows.clone())
                    .unwrap_or_default();
                this.jump_next(&targets, cx)
            }))
            .on_action(cx.listener(|this, _: &PrevHunk, _, cx| {
                let targets = this
                    .active_data()
                    .map(|d| d.hunk_rows.clone())
                    .unwrap_or_default();
                this.jump_prev(&targets, cx)
            }))
            .on_action(cx.listener(|this, _: &HunkDown, _, cx| this.step_hunk(true, cx)))
            .on_action(cx.listener(|this, _: &HunkUp, _, cx| this.step_hunk(false, cx)))
            .on_action(cx.listener(|this, _: &GoToTop, _, cx| this.jump(0, cx)))
            .on_action(cx.listener(|this, _: &GoToBottom, _, cx| {
                if let Some(last) = this.active_data().map(|d| d.rows.len().saturating_sub(1)) {
                    this.jump(last, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleView, _, cx| this.toggle_view(cx)))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.refresh(cx)))
            // Escape in the diff: close the keybindings panel, else clear the
            // selection, else move focus to the sidebar's file tree.
            .on_action(cx.listener(|this, _: &ClearSelection, window, cx| {
                if this.keybindings_visible {
                    this.keybindings_visible = false;
                    cx.notify();
                    return;
                }
                if let Some(data) = this.active_data_mut() {
                    if data.selection.take().is_some() {
                        cx.notify();
                        return;
                    }
                }
                this.focus_tree(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CopySelection, _, cx| {
                let Some(data) = this.active_data() else {
                    return;
                };
                let Some(sel) = data.selection else {
                    return;
                };
                let text = selection_text(&sel, &data.rows);
                if !text.is_empty() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.sidebar_visible = !this.sidebar_visible;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.zoom(1., false, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom(-1., false, cx)))
            .on_action(cx.listener(|this, _: &ZoomReset, _, cx| this.zoom(0., true, cx)))
            .on_action(cx.listener(|this, _: &ToggleKeybindings, _, cx| {
                this.keybindings_visible = !this.keybindings_visible;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &FocusTreeFilter, window, cx| {
                this.sidebar_visible = true;
                this.tree_filter_input
                    .update(cx, |state, cx| state.focus(window, cx));
                cx.notify();
            }))
            .child(self.render_titlebar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .when(self.sidebar_visible, |main| {
                        main.child(self.render_sidebar(window, cx))
                            .child(self.render_sidebar_resizer(cx))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .key_context("ReviewApp")
                            .track_focus(&self.focus_handle)
                            .child(pane),
                    ),
            )
            .when(self.keybindings_visible, |root| {
                root.child(self.render_keybindings(cx))
            })
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Default to the repo we're standing in; a single directory argument is
    // also accepted. Anything else is ignored (we only review local diffs).
    let repo_path = args
        .get(0)
        .filter(|arg| Path::new(arg).is_dir())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    Application::new()
        .with_assets(gpui_component_assets::Assets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            theme::apply_ui_theme(cx);
            cx.bind_keys([
                KeyBinding::new("]", NextFile, Some("ReviewApp")),
                KeyBinding::new("[", PrevFile, Some("ReviewApp")),
                KeyBinding::new("n", NextHunk, Some("ReviewApp")),
                KeyBinding::new("p", PrevHunk, Some("ReviewApp")),
                KeyBinding::new("down", HunkDown, Some("ReviewApp")),
                KeyBinding::new("up", HunkUp, Some("ReviewApp")),
                KeyBinding::new("home", GoToTop, Some("ReviewApp")),
                KeyBinding::new("end", GoToBottom, Some("ReviewApp")),
                KeyBinding::new("v", ToggleView, Some("ReviewApp")),
                KeyBinding::new("r", Refresh, Some("ReviewApp")),
                KeyBinding::new("/", FocusTreeFilter, Some("ReviewApp")),
                KeyBinding::new("ctrl-f", FocusTreeFilter, None),
                KeyBinding::new("up", TreeUp, Some("FileTree")),
                KeyBinding::new("down", TreeDown, Some("FileTree")),
                KeyBinding::new("enter", TreeOpen, Some("FileTree")),
                KeyBinding::new("left", TreeCollapse, Some("FileTree")),
                KeyBinding::new("right", TreeExpand, Some("FileTree")),
                KeyBinding::new("escape", FocusDiff, Some("FileTree")),
                KeyBinding::new("escape", ClearSelection, Some("ReviewApp")),
                KeyBinding::new("ctrl-c", CopySelection, Some("ReviewApp")),
                KeyBinding::new("ctrl-=", ZoomIn, None),
                KeyBinding::new("ctrl-+", ZoomIn, None),
                KeyBinding::new("ctrl--", ZoomOut, None),
                KeyBinding::new("ctrl-0", ZoomReset, None),
                KeyBinding::new("ctrl-b", ToggleSidebar, None),
                KeyBinding::new("ctrl-q", Quit, None),
                KeyBinding::new("ctrl-k", ToggleKeybindings, None),
            ]);
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let bounds = Bounds::centered(None, size(px(1280.), px(860.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("lgtm".into()),
                        ..TitleBar::title_bar_options()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    let view = cx.new(|cx| ReviewApp::new(repo_path, window, cx));
                    window.focus(&view.read(cx).focus_handle);
                    cx.new(|cx| Root::new(view, window, cx))
                },
            )
            .unwrap();
            cx.activate(true);
        });
}

#[cfg(test)]
mod tests {
    #[test]
    fn relevant_changes() {
        use super::is_relevant_change as rel;
        use std::path::Path;
        let root = Path::new("/r");
        assert!(rel(root, Path::new("/r/src/a.rs")));
        assert!(rel(root, Path::new("/r/.git/HEAD")));
        assert!(rel(root, Path::new("/r/.git/refs/heads/main")));
        assert!(!rel(root, Path::new("/r/.git/index")));
        assert!(!rel(root, Path::new("/r/.git/objects/ab/cd")));
        assert!(!rel(root, Path::new("/r/web/node_modules/x/y.js")));
        assert!(!rel(root, Path::new("/r/target/debug/foo")));
        assert!(!rel(root, Path::new("/elsewhere/a.rs")));
    }

    #[test]
    fn exclude_patterns() {
        use super::path_excluded as ex;
        for pat in ["__generated__", "*/__generated__/*", "__generated__/", "./__generated__"] {
            assert!(ex(pat, "__generated__/a.ts"), "{pat}");
            assert!(ex(pat, "src/x/__generated__/a.ts"), "{pat}");
            assert!(!ex(pat, "src/generated/a.ts"), "{pat}");
        }
        assert!(ex("*.snap", "tests/__snapshots__/a.snap"));
        assert!(ex("src/gen", "src/gen/a.rs"));
        assert!(!ex("src/gen", "lib/src/gener/a.rs"));
        assert!(!ex("", "a.rs"));
    }

    use super::*;
    use diff_core::Hunk;

    fn add(new_no: u32, text: &str) -> DiffRow {
        DiffRow::Added {
            new_no,
            text: text.to_string(),
            intra: Vec::new(),
        }
    }
    fn rem(old_no: u32, text: &str) -> DiffRow {
        DiffRow::Removed {
            old_no,
            text: text.to_string(),
            intra: Vec::new(),
        }
    }
    fn hunk(rows: Vec<DiffRow>) -> Hunk {
        Hunk {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
            section: String::new(),
            rows,
        }
    }
    fn file(path: &str, hunks: Vec<Hunk>) -> FileDiff {
        FileDiff {
            old_path: Some(path.to_string()),
            new_path: Some(path.to_string()),
            status: FileStatus::Modified,
            hunks,
            additions: 0,
            deletions: 0,
        }
    }

    #[test]
    fn unified_builds_headers_and_lines() {
        let diff = PrDiff {
            files: vec![file("a.rs", vec![hunk(vec![rem(1, "a"), add(1, "b")])])],
        };
        let (rows, file_rows, hunk_rows) = build_rows(&diff, ViewMode::Unified, None);
        assert_eq!(file_rows, vec![0]);
        assert_eq!(hunk_rows, vec![1]);
        assert!(matches!(rows[0], Row::FileHeader { .. }));
        assert!(matches!(rows[1], Row::HunkHeader { .. }));
        assert!(matches!(rows[2], Row::Line { .. }));
        assert!(matches!(rows[3], Row::Line { .. }));
        assert_eq!(rows.len(), 4);
    }

    #[test]
    fn split_pairs_equal_runs() {
        let diff = PrDiff {
            files: vec![file(
                "a.rs",
                vec![hunk(vec![rem(1, "a"), rem(2, "b"), add(1, "c"), add(2, "d")])],
            )],
        };
        let (rows, _, _) = build_rows(&diff, ViewMode::Split, None);
        assert_eq!(rows.len(), 4);
        assert!(matches!(&rows[2], Row::SplitLine { left: Some(_), right: Some(_) }));
        assert!(matches!(&rows[3], Row::SplitLine { left: Some(_), right: Some(_) }));
    }

    #[test]
    fn fuzzy_empty_query_keeps_order() {
        let paths = ["b.rs", "a.rs"];
        assert_eq!(fuzzy_file_matches(&paths, ""), vec![0, 1]);
        assert_eq!(fuzzy_file_matches(&paths, "b"), vec![0]);
    }

    #[test]
    fn tree_nests_dirs_first() {
        let entries = build_tree(&["src/main.rs", "README.md"]);
        assert_eq!(entries.len(), 3);
        assert!(matches!(entries[0].kind, TreeEntryKind::Dir { .. }));
    }

    #[test]
    fn max_line_chars_spans_unified_and_split() {
        let rows = vec![
            Row::Line {
                old_no: Some(1),
                new_no: Some(1),
                kind: LineKind::Context,
                text: "hello".into(),
                intra: vec![],
                syntax: vec![],
            },
            Row::SplitLine {
                left: Some(Cell {
                    no: 1,
                    kind: LineKind::Removed,
                    text: "a very long line".into(),
                    intra: vec![],
                    syntax: vec![],
                }),
                right: Some(Cell {
                    no: 1,
                    kind: LineKind::Added,
                    text: "short".into(),
                    intra: vec![],
                    syntax: vec![],
                }),
            },
        ];
        assert_eq!(widest_line(&rows), (1, 16));
    }

    #[test]
    fn selection_range_slices_ascii() {
        let row = Row::Line {
            old_no: Some(1),
            new_no: Some(1),
            kind: LineKind::Context,
            text: "hello".into(),
            intra: vec![],
            syntax: vec![],
        };
        let sel = Selection {
            side: SelSide::Unified,
            anchor: RowCol { row: 0, col: 1 },
            head: RowCol { row: 0, col: 4 },
        };
        assert_eq!(row_selection_range(&sel, 0, &row), Some(1..4));
    }
}
