//! lgtm — a minimal local git-diff viewer.
//!
//! Open it inside a repository (or pass a path) and it shows everything that
//! changed since the diff base: committed, staged, unstaged, and untracked —
//! as one reviewable diff with syntax highlighting, word-level intra-line
//! diffs, unified/split views, and a file tree.

mod buckets;
mod review;
mod theme;

use diff_core::{DiffRow, FileDiff, FileStatus, PrDiff};
use fuzzy_matcher::{skim::SkimMatcherV2, FuzzyMatcher};
use gpui::{
    actions, div, font, point, prelude::*, px, size, uniform_list, App, Application, Bounds,
    ClipboardItem, Context, FocusHandle, HighlightStyle, Hsla, KeyBinding, KeyDownEvent, Keystroke,
    ListHorizontalSizingBehavior, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Pixels, Point, ScrollStrategy, ScrollWheelEvent, SharedString, StyledText, Subscription,
    TitlebarOptions, UniformListScrollHandle, Window, WindowBounds, WindowOptions,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    input::{Escape as InputEscape, Input, InputEvent, InputState},
    kbd::Kbd,
    scroll::Scrollbar,
    tag::Tag,
    Disableable as _, Root, Sizable as _, TitleBar,
};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

const MONO: &str = "JetBrainsMono Nerd Font";

/// Diff pane font size in px, adjustable at runtime (cmd-+ / cmd-- / cmd-0).
static FONT_PX: AtomicU32 = AtomicU32::new(DEFAULT_TEXT_SIZE as u32);
const DEFAULT_TEXT_SIZE: f32 = 18.0;
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
        MarkViewed,
        OpenInEditor,
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
        EditComment,
        ClearReview,
        MoveToBucket,
        ScrollLeft,
        ScrollRight,
        CopyReview,
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

/// Files to show in the diff: those matching the sidebar's "filter files…"
/// query and not marked viewed. None when nothing is filtered.
fn shown_files(diff: &PrDiff, query: &str, hidden: &HashSet<usize>) -> Option<HashSet<usize>> {
    if query.trim().is_empty() && hidden.is_empty() {
        return None;
    }
    let paths: Vec<&str> = diff.files.iter().map(|f| f.display_path()).collect();
    Some(
        fuzzy_file_matches(&paths, query)
            .into_iter()
            .filter(|ix| !hidden.contains(ix))
            .collect(),
    )
}

/// The new-file (working tree) line number a diff row shows, if any.
fn row_new_line(row: &Row) -> Option<u32> {
    match row {
        Row::Line { new_no, .. } => *new_no,
        Row::SplitLine { right, .. } => right.as_ref().map(|cell| cell.no),
        _ => None,
    }
}

/// Fingerprint of one file's diff, so a file marked viewed comes back once
/// its changes change.
fn file_hash(file: &FileDiff) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    format!("{file:?}").hash(&mut h);
    h.finish()
}

/// Indices of files hidden from diff and tree: outside the selected bucket,
/// or marked viewed with their diff unchanged since. Also counts the viewed
/// ones (within the bucket).
fn hidden_files(
    diff: &PrDiff,
    viewed: &HashMap<String, u64>,
    buckets: &buckets::Buckets,
    filters: &[String],
    selected: &buckets::Selected,
) -> (HashSet<usize>, usize) {
    let mut viewed_count = 0;
    let hidden = diff
        .files
        .iter()
        .enumerate()
        .filter(|(_, f)| {
            let path = f.display_path();
            if !buckets.contains(selected, path, is_filtered(filters, path)) {
                return true;
            }
            let is_viewed = viewed.get(f.display_path()) == Some(&file_hash(f));
            viewed_count += usize::from(is_viewed);
            is_viewed
        })
        .map(|(ix, _)| ix)
        .collect();
    (hidden, viewed_count)
}

/// Whether `path` matches a filter pattern, putting it in Filtered out.
fn is_filtered(filters: &[String], path: &str) -> bool {
    filters.iter().any(|pat| path_excluded(pat, path))
}

/// Every path a file's commit involves: its path, plus the old one for a
/// rename.
fn file_paths(file: &FileDiff) -> impl Iterator<Item = &str> {
    let new = file.display_path();
    std::iter::once(new).chain(
        file.old_path
            .as_deref()
            .filter(|old| file.status == FileStatus::Renamed && *old != new),
    )
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
/// A hunk's `@@` line, with the first line of its review comment if any.
fn render_hunk_header(label: &SharedString, note: Option<&str>) -> gpui::AnyElement {
    div()
        .h(px(row_height()))
        .w_full()
        .flex()
        .items_center()
        .gap_4()
        .px_3()
        .bg(theme::crust())
        .text_color(theme::overlay0())
        .child(label.clone())
        .when_some(note, |row, note| {
            let first = note.lines().next().unwrap_or_default();
            let more = if note.lines().nth(1).is_some() {
                " …"
            } else {
                ""
            };
            row.child(
                div()
                    .text_color(theme::peach())
                    .child(SharedString::from(format!("\u{f075} {first}{more}"))),
            )
        })
        .into_any_element()
}

fn render_row(
    row: &Row,
    selection: Option<(SelSide, Range<usize>)>,
    split_x: Pixels,
) -> gpui::AnyElement {
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
        Row::HunkHeader { label } => render_hunk_header(label, None),
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

const TREE_ROW_HEIGHT: f32 = 36.0;

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
            .text_size(px(16.))
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
    // Click to mark the file viewed (same as Space); stops the row's own
    // click from jumping to the file.
    let viewed_button = |file_ix: usize| {
        let entity = entity.clone();
        div()
            .id(("viewed", pos))
            .flex_shrink_0()
            .px_1()
            .rounded_sm()
            .text_color(theme::overlay0())
            .hover(|s| s.text_color(theme::green()).bg(theme::surface0()))
            .child(SharedString::from("✓"))
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                entity.update(cx, |this, cx| this.mark_viewed(Some(file_ix), window, cx));
            })
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
        .when(cursor, |row| {
            row.bg(Hsla::from(theme::blue()).opacity(0.25))
        });
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
                        .child(viewed_button(*file_ix))
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
                .child(viewed_button(file_ix))
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
        .text_size(px(16.))
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
    for (ix, row) in rows.iter().enumerate().take(end.row + 1).skip(start.row) {
        if let Some(range) = row_selection_range(sel, ix, row) {
            let text = row_side_text(row, sel.side).unwrap_or_default();
            parts.push(&text[range]);
        }
    }
    parts.join("\n")
}

// --- Item data ------------------------------------------------------------

/// A row identified by content instead of index; see `ItemData::anchor_at`.
struct RowAnchor {
    path: SharedString,
    /// (old, new) line numbers, for line rows.
    lines: Option<(Option<u32>, Option<u32>)>,
    /// Rows past the file header, the fallback when the line is gone.
    from_file: usize,
}

/// One diff line with both numbers, in unified order (a split changed block
/// lists its removed lines before its added ones).
struct DiffLine {
    kind: LineKind,
    old: Option<u32>,
    new: Option<u32>,
    text: SharedString,
}

fn diff_lines(rows: &[Row]) -> Vec<DiffLine> {
    let mut out = Vec::new();
    let mut added = Vec::new();
    for row in rows {
        match row {
            Row::Line {
                old_no,
                new_no,
                kind,
                text,
                ..
            } => out.push(DiffLine {
                kind: *kind,
                old: *old_no,
                new: *new_no,
                text: text.clone(),
            }),
            Row::SplitLine {
                left: Some(l),
                right: Some(r),
            } if l.kind == LineKind::Context => {
                out.append(&mut added);
                out.push(DiffLine {
                    kind: LineKind::Context,
                    old: Some(l.no),
                    new: Some(r.no),
                    text: l.text.clone(),
                });
            }
            Row::SplitLine { left, right } => {
                if let Some(l) = left {
                    out.push(DiffLine {
                        kind: l.kind,
                        old: Some(l.no),
                        new: None,
                        text: l.text.clone(),
                    });
                }
                if let Some(r) = right {
                    added.push(DiffLine {
                        kind: r.kind,
                        old: None,
                        new: Some(r.no),
                        text: r.text.clone(),
                    });
                }
            }
            _ => {}
        }
    }
    out.append(&mut added);
    out
}

fn row_lines(row: &Row) -> Option<(Option<u32>, Option<u32>)> {
    match row {
        Row::Line { old_no, new_no, .. } => Some((*old_no, *new_no)),
        Row::SplitLine { left, right } => {
            Some((left.as_ref().map(|c| c.no), right.as_ref().map(|c| c.no)))
        }
        _ => None,
    }
}

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
    /// Files hidden from diff and tree: outside the selected bucket, or
    /// marked viewed (unchanged since).
    hidden: HashSet<usize>,
    /// How many of `hidden` are hidden as viewed.
    viewed_count: usize,
    /// Each changed path's reviewed state, for commits to check against.
    states: HashMap<String, git::Expected>,
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

    /// Pins row `ix` to content (its file and line numbers) rather than its
    /// index, so it can be found again after the rows are rebuilt.
    fn anchor_at(&self, ix: usize) -> Option<RowAnchor> {
        let (path, file) = self.file_of(ix)?;
        Some(RowAnchor {
            path,
            lines: self.rows.get(ix).and_then(row_lines),
            from_file: ix - file.start,
        })
    }

    /// Where `anchor` landed in the current rows: the same line if it still
    /// exists, else the first line past it, else the same offset into its
    /// file. None when the file is gone.
    fn resolve_anchor(&self, anchor: &RowAnchor) -> Option<usize> {
        let Range { start, end } = self.file_range(&anchor.path)?;
        let fallback = (start + anchor.from_file).min(end.saturating_sub(1));
        let Some((old, new)) = anchor.lines else {
            return Some(fallback);
        };
        let lines = (start..end).filter_map(|ix| Some((ix, row_lines(&self.rows[ix])?)));
        let exact = lines.clone().find(|&(_, (o, n))| match new {
            Some(new) => n == Some(new),
            None => o == old,
        });
        let after = || {
            lines.clone().find(|&(_, (o, n))| match (new, n, old, o) {
                (Some(t), Some(n), _, _) => n >= t,
                (None, _, Some(t), Some(o)) => o >= t,
                _ => false,
            })
        };
        Some(exact.or_else(after).map_or(fallback, |(ix, _)| ix))
    }

    /// Rows from the header at `start` up to the next file's header.
    /// `file_rows` has an entry per diff file, and a hidden file's entry
    /// repeats the next shown header or points past the end, so only
    /// strictly greater entries end a file.
    fn file_end(&self, start: usize) -> usize {
        self.file_rows
            .iter()
            .copied()
            .filter(|&f| f > start)
            .min()
            .unwrap_or(self.rows.len())
            .min(self.rows.len())
    }

    /// The row range of the shown file at `path`.
    fn file_range(&self, path: &str) -> Option<Range<usize>> {
        let start = self.file_rows.iter().copied().find(|&f| {
            matches!(self.rows.get(f), Some(Row::FileHeader { path: p, .. }) if p.as_ref() == path)
        })?;
        Some(start..self.file_end(start))
    }

    /// The file containing row `ix`: its path and row range.
    fn file_of(&self, ix: usize) -> Option<(SharedString, Range<usize>)> {
        let start = self.file_rows.iter().copied().filter(|&f| f <= ix).max()?;
        match self.rows.get(start)? {
            Row::FileHeader { path, .. } => Some((path.clone(), start..self.file_end(start))),
            _ => None,
        }
    }

    /// What a comment on `rows` would cover: the file, the line span, and
    /// the diff lines as text. `side` restricts to one split side; otherwise
    /// the span uses working-tree numbers unless only removed lines are
    /// involved. `changed_only` leaves context lines out of the span.
    fn comment_target(
        &self,
        rows: Range<usize>,
        side: Option<review::Side>,
        changed_only: bool,
    ) -> Option<(SharedString, review::Span, String)> {
        use review::Side;
        let (path, file) = self.file_of(rows.start)?;
        let rows = rows.start.max(file.start)..rows.end.min(file.end);
        let lines = diff_lines(&self.rows[rows]);
        let no = |l: &DiffLine, side| match side {
            Side::New => l.new,
            Side::Old => l.old,
        };
        let span_side = side.unwrap_or_else(|| {
            let has_new = lines
                .iter()
                .any(|l| l.new.is_some() && (!changed_only || l.kind == LineKind::Added));
            if has_new {
                Side::New
            } else {
                Side::Old
            }
        });
        let nos = lines
            .iter()
            .filter(|l| !changed_only || l.kind != LineKind::Context)
            .filter_map(|l| no(l, span_side));
        let start = nos.clone().min()?;
        let end = nos.max()?;
        const MAX_CODE_LINES: usize = 60;
        let shown: Vec<&DiffLine> = lines
            .iter()
            .filter(|l| side.is_none_or(|side| no(l, side).is_some()))
            .collect();
        let mut code: Vec<String> = shown
            .iter()
            .take(MAX_CODE_LINES)
            .map(|l| {
                let mark = match l.kind {
                    LineKind::Added => '+',
                    LineKind::Removed => '-',
                    LineKind::Context => ' ',
                };
                format!("{mark}{}", l.text)
            })
            .collect();
        if shown.len() > MAX_CODE_LINES {
            code.push(format!("… {} more lines", shown.len() - MAX_CODE_LINES));
        }
        let span = review::Span {
            side: span_side,
            start,
            end,
        };
        Some((path, span, code.join("\n")))
    }

    /// The row showing the last line of `span` in `path`, to place the
    /// comment editor under.
    fn span_end_row(&self, path: &str, span: &review::Span) -> Option<usize> {
        self.file_range(path)?.rev().find(|&ix| {
            row_lines(&self.rows[ix]).is_some_and(|(old, new)| {
                let no = match span.side {
                    review::Side::New => new,
                    review::Side::Old => old,
                };
                no.is_some_and(|no| (span.start..=span.end).contains(&no))
            })
        })
    }

    fn rebuild_tree(&mut self) {
        let shown: Vec<usize> = (0..self.diff.files.len())
            .filter(|ix| !self.hidden.contains(ix))
            .collect();
        let paths: Vec<&str> = shown
            .iter()
            .map(|&ix| self.diff.files[ix].display_path())
            .collect();
        let mut tree = build_tree(&paths);
        for entry in &mut tree {
            if let TreeEntryKind::File { file_ix } = &mut entry.kind {
                *file_ix = shown[*file_ix];
            }
        }
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
    hidden: HashSet<usize>,
    viewed_count: usize,
    states: HashMap<String, git::Expected>,
    rows: Vec<Row>,
    file_rows: Vec<usize>,
    hunk_rows: Vec<usize>,
    mode: ViewMode,
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

/// Filtered out's patterns on startup; each shows as a removable tag.
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
    filters: &[String],
    query: &str,
    viewed: &HashMap<String, u64>,
    buckets: &buckets::Buckets,
    selected: &buckets::Selected,
) -> anyhow::Result<Loaded> {
    // Uncommitted changes only: the view is a review → commit loop, so a
    // commit clears it.
    let src = git::resolve_local_with_base(path, Some("HEAD"))?;
    let patch = git::diff_patch(&src)?;
    let patch_hash = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        patch.hash(&mut h);
        h.finish()
    };
    let diff = diff_core::parse_patch(&patch);
    let (hidden, viewed_count) = hidden_files(&diff, viewed, buckets, filters, selected);
    let (rows, file_rows, hunk_rows) =
        build_rows(&diff, mode, shown_files(&diff, query, &hidden).as_ref());
    let paths: Vec<String> = diff
        .files
        .iter()
        .flat_map(file_paths)
        .map(str::to_string)
        .collect();
    let states = git::worktree_state(&src.repo_root, &paths);
    Ok(Loaded {
        src,
        diff,
        hidden,
        viewed_count,
        states,
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
            bar.child(
                div()
                    .text_color(theme::blue())
                    .child(SharedString::from(format!(
                        "hunk {}/{}",
                        pos + 1,
                        data.hunk_rows.len()
                    ))),
            )
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
    /// Hash of the patch behind the installed rows, plus the mode/excludes it
    /// was built with; a refetch that matches is dropped so watcher noise
    /// doesn't reset scroll or selection.
    installed_key: Option<(u64, ViewMode)>,
    _watcher: Option<notify::RecommendedWatcher>,
    focus_handle: FocusHandle,
    /// Keyboard focus for the sidebar file tree, and its cursor (a position
    /// in `tree_list_rows`).
    tree_focus: FocusHandle,
    tree_cursor: usize,
    /// Files marked viewed: path → fingerprint of its diff when marked.
    viewed: HashMap<String, u64>,
    drag_anchor: Option<(SelSide, RowCol)>,
    char_width: Option<Pixels>,
    repo_path: PathBuf,
    review: review::Review,
    comment_input: gpui::Entity<InputState>,
    /// The comment open in the inline editor.
    editing: Option<Editing>,
    /// The report was just copied; cleared when the review changes.
    review_copied: bool,
    commit_input: gpui::Entity<InputState>,
    buckets: buckets::Buckets,
    /// The bucket shown, and what Commit commits.
    selected: buckets::Selected,
    bucket_input: gpui::Entity<InputState>,
    /// Naming a new bucket (the input shows in the tabs row).
    creating_bucket: bool,
    /// Files waiting for the bucket picker (`m`).
    moving: Option<Vec<String>>,
    picker_focus: FocusHandle,
    committing: bool,
    /// Result of the last commit: Ok(summary) or Err(message).
    commit_status: Option<Result<SharedString, SharedString>>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Default)]
struct BucketCounts {
    /// Everything but Filtered out.
    all: usize,
    default: usize,
    filtered: usize,
    named: Vec<usize>,
}

/// A comment being written or edited: the review entry it replaces, if any,
/// and what it's pinned to.
struct Editing {
    ix: Option<usize>,
    path: SharedString,
    span: review::Span,
    code: String,
}

impl ReviewApp {
    fn new(repo_path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let tree_filter_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("filter files…"));
        let exclude_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("add pattern, e.g. __generated__ (enter)")
        });
        let comment_input = cx.new(|cx| {
            InputState::new(window, cx)
                .auto_grow(3, 12)
                .placeholder("comment on these lines… (esc to save, empty to delete)")
        });
        let commit_input = cx.new(|cx| {
            InputState::new(window, cx)
                .auto_grow(2, 8)
                .placeholder("commit message (ctrl-enter to commit)")
        });
        let bucket_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("bucket name, enter to create"));
        let _subscriptions = vec![
            cx.subscribe_in(
                &bucket_input,
                window,
                |this, _, event: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        this.create_bucket(window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &commit_input,
                window,
                |this, input, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { secondary: true } => this.commit(window, cx),
                    // Typing a new message dismisses the last result (the
                    // box emptying itself after a commit doesn't).
                    InputEvent::Change
                        if !input.read(cx).value().is_empty()
                            && this.commit_status.take().is_some() =>
                    {
                        cx.notify()
                    }
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &comment_input,
                window,
                |this, _, event: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { secondary: true } = event {
                        this.close_comment(window, cx);
                    }
                },
            ),
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
        let buckets = buckets::Buckets::load(&repo_path);
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
            installed_key: None,
            _watcher: None,
            focus_handle: cx.focus_handle(),
            tree_focus: cx.focus_handle(),
            tree_cursor: 0,
            viewed: HashMap::new(),
            drag_anchor: None,
            char_width: None,
            repo_path,
            review: review::Review::default(),
            comment_input,
            editing: None,
            review_copied: false,
            commit_input,
            buckets,
            selected: buckets::Selected::All,
            bucket_input,
            creating_bucket: false,
            moving: None,
            picker_focus: cx.focus_handle(),
            committing: false,
            commit_status: None,
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
        let viewed = self.viewed.clone();
        let buckets = self.buckets.clone();
        let selected = self.selected.clone();
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn({
                    let exclude = exclude.clone();
                    let query = query.clone();
                    async move {
                        fetch_item(&repo, mode, &exclude, &query, &viewed, &buckets, &selected)
                    }
                })
                .await;
            this.update(cx, |app, cx| {
                app.reloading = false;
                match fetched {
                    Ok(loaded) => {
                        let key = Some((loaded.patch_hash, loaded.mode));
                        let unchanged = key == app.installed_key
                            && matches!(&app.state, LoadState::Ready(d) if d.mode == loaded.mode);
                        if !unchanged {
                            app.installed_key = key;
                            app.install(loaded, cx);
                            // The file filter changed while fetching.
                            if app.tree_filter_input.read(cx).value().as_ref() != query {
                                app.apply_file_filter(cx);
                            }
                            // Filters changed while fetching.
                            if app.effective_exclude(cx) != exclude {
                                app.apply_viewed(cx);
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
            hidden,
            viewed_count,
            states,
            rows,
            file_rows,
            hunk_rows,
            mode,
            patch_hash: _,
        } = loaded;
        if self._watcher.is_none() {
            self.review = review::Review::load(&src.repo_root);
            self.watch_repo(src.repo_root.clone(), cx);
        }
        let (additions, deletions) = diff
            .files
            .iter()
            .fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions));
        self.refresh_error = None;
        match &mut self.state {
            LoadState::Ready(data) => {
                data.src = src;
                data.diff = diff;
                data.hidden = hidden;
                data.viewed_count = viewed_count;
                data.states = states;
                data.additions = additions;
                data.deletions = deletions;
                // Keep the viewport and cursor on the same content: changes
                // above them shift row indices, which would make the view jump.
                let rh = row_height();
                let handle = data.scroll.0.borrow().base_handle.clone();
                let offset = handle.offset();
                let top_px = (-f32::from(offset.y)).max(0.);
                let top_ix = (top_px / rh) as usize;
                let top_anchor = data.anchor_at(top_ix);
                let cursor_anchor = data.anchor_at(data.cursor);
                let (old_top, old_cursor) = (top_ix, data.cursor);
                data.set_rows((rows, file_rows, hunk_rows));
                let last = data.rows.len().saturating_sub(1);
                let new_top = top_anchor
                    .and_then(|a| data.resolve_anchor(&a))
                    .unwrap_or(old_top)
                    .min(last);
                if new_top != old_top && data.scroll.0.borrow().deferred_scroll_to_item.is_none() {
                    let frac = top_px - old_top as f32 * rh;
                    handle.set_offset(point(offset.x, px(-(new_top as f32 * rh + frac))));
                }
                data.cursor = cursor_anchor
                    .and_then(|a| data.resolve_anchor(&a))
                    .unwrap_or(old_cursor)
                    .min(last);
                data.selection = None;
                data.rebuild_tree();
            }
            _ => {
                let (widest_row_ix, max_line_chars) = widest_line(&rows);
                let mut data = Box::new(ItemData {
                    src,
                    diff,
                    hidden,
                    viewed_count,
                    states,
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

    /// Opens the inline editor on the selected lines, else on the cursor's
    /// hunk, showing the comment already there if any.
    fn open_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_comment(cx);
        let Some(data) = self.active_data() else {
            return;
        };
        let target = match data.selection {
            Some(sel) => {
                let (from, to) = sel.ordered();
                let side = match sel.side {
                    SelSide::Left => Some(review::Side::Old),
                    SelSide::Right => Some(review::Side::New),
                    SelSide::Unified => None,
                };
                data.comment_target(from.row..to.row + 1, side, false)
            }
            None => data
                .current_hunk()
                .and_then(|(_, start, end)| data.comment_target(start..end, None, true)),
        };
        let Some((path, mut span, mut code)) = target else {
            return;
        };
        let ix = self.review.find(&path, &span);
        let body = match ix.map(|ix| &self.review.comments[ix]) {
            Some(existing) => {
                // Opening from the hunk keeps a narrower comment's own lines.
                if data.selection.is_none() {
                    span = existing.span;
                    code = existing.code.clone();
                }
                existing.body.clone()
            }
            None => String::new(),
        };
        self.editing = Some(Editing {
            ix,
            path,
            span,
            code,
        });
        self.comment_input.update(cx, |input, cx| {
            input.set_value(body, window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    /// Saves the open comment (an empty one is deleted) and returns focus to
    /// the diff.
    fn close_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.save_comment(cx) {
            return;
        }
        if let Some(data) = self.active_data_mut() {
            data.selection = None;
        }
        window.focus(&self.focus_handle);
        cx.notify();
    }

    /// Stores the open comment and closes the editor; false if none was open.
    fn save_comment(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(Editing {
            ix,
            path,
            span,
            code,
        }) = self.editing.take()
        else {
            return false;
        };
        let body = self.comment_input.read(cx).value().trim().to_string();
        self.review.set(
            ix,
            review::Comment {
                path: path.to_string(),
                span,
                body,
                code,
            },
        );
        self.review_copied = false;
        true
    }

    /// Stages everything and commits it with the message box's text.
    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let message = self.commit_input.read(cx).value().trim().to_string();
        if message.is_empty() || self.committing {
            return;
        }
        let Some(root) = self.active_data().map(|data| data.src.repo_root.clone()) else {
            return;
        };
        let files = self.bucket_commit_files(cx);
        if files.is_empty() {
            return;
        }
        self.committing = true;
        self.commit_status = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let paths: Vec<String> = files.iter().map(|(p, _)| p.clone()).collect();
            let result = cx
                .background_spawn(async move { git::commit_files(&root, &files, &message) })
                .await;
            this.update_in(cx, |app, window, cx| {
                app.committing = false;
                match result {
                    Ok(done) => {
                        let status = match done.warning {
                            Some(warning) => format!("{}\n{warning}", done.summary),
                            None => done.summary,
                        };
                        app.commit_status = Some(Ok(status.into()));
                        // Its files left the diff: drop their bucket entries
                        // and the comments about them.
                        app.buckets.forget(paths.iter().map(String::as_str));
                        app.review.forget(paths.iter().map(String::as_str));
                        app.editing = None;
                        app.review_copied = false;
                        app.commit_input
                            .update(cx, |input, cx| input.set_value("", window, cx));
                        app.refresh(cx);
                    }
                    Err(err) => app.commit_status = Some(Err(format!("{err:#}").into())),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The selected bucket's files with their reviewed states, viewed ones
    /// included.
    fn bucket_commit_files(&self, cx: &App) -> Vec<(String, git::Expected)> {
        let Some(data) = self.active_data() else {
            return Vec::new();
        };
        let filters = self.effective_exclude(cx);
        data.diff
            .files
            .iter()
            .filter(|f| {
                let path = f.display_path();
                self.buckets
                    .contains(&self.selected, path, is_filtered(&filters, path))
            })
            .flat_map(file_paths)
            .map(|path| {
                // Unfingerprinted (it was changing): a blob that can't match.
                let state = data
                    .states
                    .get(path)
                    .cloned()
                    .unwrap_or(git::Expected::Blob(String::new()));
                (path.to_string(), state)
            })
            .collect()
    }

    /// How many diff files each bucket holds: (All, Default, per name).
    /// How many diff files each bucket holds.
    fn bucket_counts(&self, cx: &App) -> BucketCounts {
        let mut counts = BucketCounts {
            named: vec![0; self.buckets.names.len()],
            ..Default::default()
        };
        let Some(data) = self.active_data() else {
            return counts;
        };
        let filters = self.effective_exclude(cx);
        for f in &data.diff.files {
            let path = f.display_path();
            if is_filtered(&filters, path) {
                counts.filtered += 1;
                continue;
            }
            counts.all += 1;
            match self.buckets.bucket_of(path) {
                Some(name) => {
                    if let Some(i) = self.buckets.names.iter().position(|n| n == name) {
                        counts.named[i] += 1;
                    }
                }
                None => counts.default += 1,
            }
        }
        counts
    }

    fn select_bucket(&mut self, selected: buckets::Selected, cx: &mut Context<Self>) {
        if self.selected == selected {
            return;
        }
        self.selected = selected;
        self.commit_status = None;
        self.apply_viewed(cx);
        self.tree_cursor = 0;
        if let Some(data) = self.active_data() {
            data.tree_scroll.scroll_to_item(0, ScrollStrategy::Top);
        }
        self.jump(0, cx);
    }

    fn create_bucket(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.bucket_input.read(cx).value().trim().to_string();
        if !self.buckets.create(&name) {
            return;
        }
        self.creating_bucket = false;
        self.bucket_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        // Named from the picker: the files being moved go straight in.
        if self.moving.is_some() {
            self.move_to_bucket(Some(name), window, cx);
        } else {
            window.focus(&self.focus_handle);
            cx.notify();
        }
    }

    fn start_new_bucket(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_visible = true;
        self.creating_bucket = true;
        self.bucket_input
            .update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn delete_bucket(&mut self, name: &str, cx: &mut Context<Self>) {
        self.buckets.delete(name);
        self.selected = buckets::Selected::All;
        self.apply_viewed(cx);
        cx.notify();
    }

    /// `m`: pick a bucket for the file being read, or the file or folder
    /// under the tree cursor.
    fn open_bucket_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let files = if self.tree_focus.is_focused(window) {
            self.tree_cursor_files(cx)
        } else {
            self.active_data()
                .and_then(|data| data.viewed_file())
                .into_iter()
                .collect()
        };
        let Some(data) = self.active_data() else {
            return;
        };
        // Filtered-out files belong to their patterns, not to a bucket.
        let filters = self.effective_exclude(cx);
        let paths: Vec<String> = files
            .iter()
            .filter_map(|&ix| data.diff.files.get(ix))
            .map(|f| f.display_path())
            .filter(|path| !is_filtered(&filters, path))
            .map(str::to_string)
            .collect();
        if paths.is_empty() {
            return;
        }
        self.moving = Some(paths);
        window.focus(&self.picker_focus);
        cx.notify();
    }

    fn close_bucket_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.moving = None;
        self.creating_bucket = false;
        window.focus(&self.focus_handle);
        cx.notify();
    }

    /// Puts the picked files in bucket `name` (None: Default); they leave
    /// the view if another bucket is shown.
    fn move_to_bucket(
        &mut self,
        name: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(paths) = self.moving.take() else {
            return;
        };
        self.buckets
            .assign(paths.iter().map(String::as_str), name.as_deref());
        window.focus(&self.focus_handle);
        let last = self.active_data().and_then(|data| {
            data.diff
                .files
                .iter()
                .rposition(|f| paths.iter().any(|p| p == f.display_path()))
        });
        match last {
            Some(last) => self.hide_and_move_on(last, cx),
            None => cx.notify(),
        }
    }

    /// The `m` picker: digits pick a bucket, `n` names a new one.
    fn render_bucket_picker(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let paths = self.moving.as_ref()?;
        let what: SharedString = match paths.as_slice() {
            [one] => one.clone().into(),
            many => format!("{} files", many.len()).into(),
        };
        let mut options: Vec<(SharedString, Option<String>)> = vec![("Default".into(), None)];
        options.extend(
            self.buckets
                .names
                .iter()
                .take(8)
                .map(|n| (n.clone().into(), Some(n.clone()))),
        );
        let row = |key: SharedString, label: SharedString| {
            div()
                .flex()
                .gap_3()
                .px_2()
                .py_1()
                .rounded(px(4.))
                .cursor_pointer()
                .hover(|s| s.bg(theme::surface0()))
                .child(div().w(px(16.)).text_color(theme::blue()).child(key))
                .child(label)
        };
        let mut list = div().flex().flex_col().gap_1();
        for (i, (label, name)) in options.into_iter().enumerate() {
            list = list.child(
                row(format!("{}", i + 1).into(), label)
                    .id(("bucket-pick", i))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.move_to_bucket(name.clone(), window, cx)
                    })),
            );
        }
        list = list.child(
            row("n".into(), "new bucket…".into())
                .id("bucket-pick-new")
                .on_click(cx.listener(|this, _, window, cx| this.start_new_bucket(window, cx))),
        );
        Some(
            div()
                .absolute()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(Hsla::from(theme::crust()).opacity(0.6))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, window, cx| {
                        this.close_bucket_picker(window, cx)
                    }),
                )
                .child(
                    div()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .track_focus(&self.picker_focus)
                        .key_context("BucketPicker")
                        .on_action(cx.listener(|this, _: &InputEscape, window, cx| {
                            this.close_bucket_picker(window, cx)
                        }))
                        .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                            // Typing a new bucket's name goes to its input.
                            if this.creating_bucket {
                                return;
                            }
                            let key = event.keystroke.key.as_str();
                            match key {
                                "escape" => this.close_bucket_picker(window, cx),
                                "n" => this.start_new_bucket(window, cx),
                                _ => {
                                    let Some(d) = key.parse::<usize>().ok().filter(|d| *d >= 1)
                                    else {
                                        return;
                                    };
                                    if d == 1 {
                                        this.move_to_bucket(None, window, cx);
                                    } else if let Some(name) =
                                        this.buckets.names.get(d - 2).cloned()
                                    {
                                        this.move_to_bucket(Some(name), window, cx);
                                    }
                                }
                            }
                            cx.stop_propagation();
                        }))
                        .w(px(360.))
                        .p_3()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .bg(theme::mantle())
                        .border_1()
                        .border_color(theme::surface0())
                        .rounded(px(8.))
                        .shadow_lg()
                        .text_size(px(16.))
                        .child(
                            div()
                                .text_color(theme::overlay0())
                                .truncate()
                                .child(SharedString::from(format!("move {what} to"))),
                        )
                        .child(list)
                        .when(self.creating_bucket, |col| {
                            col.child(Input::new(&self.bucket_input))
                        }),
                )
                .into_any_element(),
        )
    }

    /// Bucket tabs: All, Default, each named bucket, and `+`.
    fn render_bucket_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use buckets::Selected;
        let BucketCounts {
            all,
            default,
            filtered,
            named,
        } = self.bucket_counts(cx);
        let tab = |id: SharedString, label: String, n: Option<usize>, selected: bool| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap_1()
                .px_2()
                .py(px(2.))
                .rounded(px(4.))
                .cursor_pointer()
                .when(selected, |t| {
                    t.bg(theme::surface0()).text_color(theme::blue())
                })
                .when(!selected, |t| {
                    t.text_color(theme::overlay0())
                        .hover(|s| s.text_color(theme::text()))
                })
                .child(SharedString::from(label))
                .when_some(n, |t, n| {
                    t.child(
                        div()
                            .text_color(theme::overlay0())
                            .child(SharedString::from(n.to_string())),
                    )
                })
        };
        let mut row = div()
            .px_2()
            .pb_1()
            .flex()
            .flex_wrap()
            .gap_1()
            .text_size(px(16.))
            .child(
                tab(
                    "bucket-all".into(),
                    "All".into(),
                    Some(all),
                    self.selected == Selected::All,
                )
                .on_click(cx.listener(|this, _, _, cx| this.select_bucket(Selected::All, cx))),
            )
            .child(
                tab(
                    "bucket-default".into(),
                    "Default".into(),
                    Some(default),
                    self.selected == Selected::Default,
                )
                .on_click(cx.listener(|this, _, _, cx| this.select_bucket(Selected::Default, cx))),
            );
        for (i, name) in self.buckets.names.iter().enumerate() {
            let selected = self.selected == Selected::Named(name.clone());
            let pick = name.clone();
            let mut t = tab(
                format!("bucket-{i}").into(),
                name.clone(),
                Some(named[i]),
                selected,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.select_bucket(Selected::Named(pick.clone()), cx)
            }));
            if selected {
                let name = name.clone();
                t = t.child(
                    div()
                        .id(("bucket-delete", i))
                        .text_color(theme::overlay0())
                        .hover(|s| s.text_color(theme::red()))
                        .child("×")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.delete_bucket(&name, cx)
                        })),
                );
            }
            row = row.child(t);
        }
        row.child(
            tab(
                "bucket-filtered".into(),
                "Filtered out".into(),
                Some(filtered),
                self.selected == Selected::Filtered,
            )
            .on_click(cx.listener(|this, _, _, cx| this.select_bucket(Selected::Filtered, cx))),
        )
        .child(
            tab("bucket-new".into(), "+".into(), None, false)
                .on_click(cx.listener(|this, _, window, cx| this.start_new_bucket(window, cx))),
        )
        .when(self.creating_bucket && self.moving.is_none(), |row| {
            row.child(div().w_full().child(Input::new(&self.bucket_input)))
        })
    }

    fn copy_review(&mut self, cx: &mut Context<Self>) {
        self.save_comment(cx);
        if self.review.comments.is_empty() {
            return;
        }
        let report = self.review.to_markdown();
        cx.write_to_clipboard(ClipboardItem::new_string(report));
        self.review_copied = true;
        cx.notify();
    }

    fn clear_review(&mut self, cx: &mut Context<Self>) {
        self.editing = None;
        self.review.clear();
        self.review_copied = false;
        cx.notify();
    }

    /// The inline comment editor, floated under the lines it's about.
    fn render_comment_editor(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let editing = self.editing.as_ref()?;
        let data = self.active_data()?;
        let rh = row_height();
        let handle = data.scroll.0.borrow().base_handle.clone();
        let viewport = f32::from(handle.bounds().size.height);
        let row = data
            .span_end_row(&editing.path, &editing.span)
            .unwrap_or(data.cursor);
        const EDITOR_H: f32 = 260.;
        let top = ((row + 1) as f32 * rh + f32::from(handle.offset().y))
            .min(viewport - EDITOR_H)
            .max(0.);
        let is_new = editing.ix.is_none();
        Some(
            div()
                .absolute()
                .top(px(top))
                .left(px(48.))
                .right(px(24.))
                .max_w(px(760.))
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .bg(theme::mantle())
                .border_1()
                .border_color(theme::peach())
                .rounded(px(6.))
                .shadow_lg()
                .text_size(px(13.))
                .on_action(
                    cx.listener(|this, _: &InputEscape, window, cx| this.close_comment(window, cx)),
                )
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .text_color(theme::overlay0())
                        .child(SharedString::from(format!(
                            "{} {}",
                            if is_new {
                                "new comment ·"
                            } else {
                                "comment ·"
                            },
                            review::Comment {
                                path: editing.path.to_string(),
                                span: editing.span,
                                body: String::new(),
                                code: String::new(),
                            }
                            .location()
                        )))
                        .child("esc / ctrl-enter save"),
                )
                .child(Input::new(&self.comment_input))
                .into_any_element(),
        )
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

    /// Left/right keys: scroll the diff text sideways by a few characters,
    /// when it's wider than the pane.
    fn scroll_x_by_key(&mut self, right: bool, window: &Window, cx: &mut Context<Self>) {
        let step = f32::from(self.char_width(window)) * 8.;
        let delta = if right { -step } else { step };
        let Some(data) = self.active_data() else {
            return;
        };
        if data.mode == ViewMode::Split {
            if self.scroll_split_x(delta, window) {
                cx.notify();
            }
            return;
        }
        let handle = data.scroll.0.borrow().base_handle.clone();
        let offset = handle.offset();
        let max = f32::from(handle.max_offset().width);
        let x = (f32::from(offset.x) + delta).clamp(-max, 0.);
        if x != f32::from(offset.x) {
            handle.set_offset(point(px(x), offset.y));
            cx.notify();
        }
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
            data.set_rows(build_rows(
                &data.diff,
                data.mode,
                shown_files(&data.diff, &query, &data.hidden).as_ref(),
            ));
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
        data.set_rows(build_rows(
            &data.diff,
            data.mode,
            shown_files(&data.diff, &query, &data.hidden).as_ref(),
        ));
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
                Some((_, start, end)) if viewport > 0. && (end - start) as f32 * rh < viewport => {
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
        self.apply_filters(cx);
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
            this.update(cx, |this, cx| this.apply_filters(cx)).ok();
        }));
    }

    fn remove_exclude(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.exclude.remove(ix);
        self.apply_filters(cx);
    }

    /// Re-sorts files into or out of Filtered out after the patterns changed.
    fn apply_filters(&mut self, cx: &mut Context<Self>) {
        let filters = self.effective_exclude(cx);
        if filters != self.applied_exclude {
            self.applied_exclude = filters;
            self.apply_viewed(cx);
            cx.notify();
        }
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
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    this.titlebar_dragging = true;
                    cx.stop_propagation();
                }),
            )
            .on_mouse_move(cx.listener(|this, _: &MouseMoveEvent, window, _| {
                if this.titlebar_dragging {
                    this.titlebar_dragging = false;
                    window.start_window_move();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, _| {
                    this.titlebar_dragging = false;
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, _| {
                    this.titlebar_dragging = false;
                }),
            )
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
                .filter(|ix| !data.hidden.contains(ix))
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
            rows.iter()
                .position(|&row| self.tree_row_file(row) == Some(file))
        }) {
            self.tree_cursor = pos;
        }
        self.tree_cursor = self.tree_cursor.min(rows.len().saturating_sub(1));
        window.focus(&self.tree_focus);
        cx.notify();
    }

    /// Where showing `file` puts the cursor: its first hunk, else its header.
    /// A hidden file's entry already points at the next shown file.
    fn file_landing_row(&self, file: usize) -> Option<usize> {
        let data = self.active_data()?;
        let header = *data.file_rows.get(file)?;
        if header >= data.rows.len() {
            return data.rows.len().checked_sub(1);
        }
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
            .find(|&ix| ix >= header && ix < next_file);
        Some(first_hunk.unwrap_or(header))
    }

    /// Recompute what's hidden and rebuild rows and tree after `viewed`
    /// changed.
    fn apply_viewed(&mut self, cx: &mut Context<Self>) {
        let query = self.tree_filter_input.read(cx).value().to_string();
        let viewed = self.viewed.clone();
        let filters = self.effective_exclude(cx);
        let (buckets, selected) = (&self.buckets, &self.selected);
        if let LoadState::Ready(data) = &mut self.state {
            (data.hidden, data.viewed_count) =
                hidden_files(&data.diff, &viewed, buckets, &filters, selected);
            data.set_rows(build_rows(
                &data.diff,
                data.mode,
                shown_files(&data.diff, &query, &data.hidden).as_ref(),
            ));
            data.selection = None;
            data.rebuild_tree();
        }
    }

    /// Space: mark the file under the tree cursor (tree focused) or the file
    /// being read (diff focused) as viewed, hiding it and moving on to the
    /// next file.
    fn mark_viewed(&mut self, file: Option<usize>, window: &Window, cx: &mut Context<Self>) {
        let files: Vec<usize> = match file {
            Some(file) => vec![file],
            None if self.tree_focus.is_focused(window) => self.tree_cursor_files(cx),
            None => self
                .active_data()
                .and_then(|data| data.viewed_file())
                .into_iter()
                .collect(),
        };
        let Some(&last) = files.iter().max() else {
            return;
        };
        let Some(entries) = self.active_data().map(|data| {
            files
                .iter()
                .filter_map(|&ix| data.diff.files.get(ix))
                .map(|f| (f.display_path().to_string(), file_hash(f)))
                .collect::<Vec<_>>()
        }) else {
            return;
        };
        self.viewed.extend(entries);
        self.hide_and_move_on(last, cx);
    }

    /// After files up to `last` were hidden (viewed, or moved out of the
    /// shown bucket): rebuild, and continue at the next shown file.
    fn hide_and_move_on(&mut self, last: usize, cx: &mut Context<Self>) {
        self.apply_viewed(cx);
        let rows = self.tree_list_rows(cx);
        self.tree_cursor = self.tree_cursor.min(rows.len().saturating_sub(1));
        // Continue after the last hidden file.
        match self.file_landing_row(last) {
            Some(row) => self.jump(row, cx),
            None => cx.notify(),
        }
    }

    /// Files under the tree cursor: the file itself, or every shown file
    /// inside the directory it's on.
    fn tree_cursor_files(&self, cx: &App) -> Vec<usize> {
        let rows = self.tree_list_rows(cx);
        let Some(&row) = rows.get(self.tree_cursor) else {
            return Vec::new();
        };
        if let Some(file) = self.tree_row_file(row) {
            return vec![file];
        }
        let (TreeListRow::Entry(entry_ix), Some(data)) = (row, self.active_data()) else {
            return Vec::new();
        };
        let Some(TreeEntryKind::Dir { path }) = data.tree.get(entry_ix).map(|e| &e.kind) else {
            return Vec::new();
        };
        let prefix = format!("{path}/");
        data.diff
            .files
            .iter()
            .enumerate()
            .filter(|(ix, f)| !data.hidden.contains(ix) && f.display_path().starts_with(&prefix))
            .map(|(ix, _)| ix)
            .collect()
    }

    /// `z`: open the file in Zed at the line being looked at — the selection
    /// (with its column) or the cursor row, stepping to the nearest line that
    /// exists in the working tree (removed lines and headers have none). From
    /// the file tree, the file's first changed line.
    fn open_in_editor(&mut self, window: &Window, cx: &mut Context<Self>) {
        let from_tree = self.tree_focus.is_focused(window);
        let tree_file = from_tree
            .then(|| {
                let rows = self.tree_list_rows(cx);
                rows.get(self.tree_cursor)
                    .and_then(|&row| self.tree_row_file(row))
            })
            .flatten();
        let Some(data) = self.active_data() else {
            return;
        };
        let (row, col) = match (tree_file, data.selection) {
            (Some(file), _) => match data.file_rows.get(file) {
                Some(&header) => (header, None),
                None => return,
            },
            (None, Some(sel)) => (
                sel.head.row,
                (sel.side != SelSide::Left).then_some(sel.head.col + 1),
            ),
            (None, None) => (data.cursor, None),
        };
        let Some(file) = data.file_rows.iter().rposition(|&ix| ix <= row) else {
            return;
        };
        let start = data.file_rows[file];
        let end = data
            .file_rows
            .get(file + 1)
            .copied()
            .unwrap_or(data.rows.len())
            .min(data.rows.len());
        let row = row.min(end.saturating_sub(1));
        let line = (row..end)
            .find_map(|ix| row_new_line(&data.rows[ix]))
            .or_else(|| {
                (start..row)
                    .rev()
                    .find_map(|ix| row_new_line(&data.rows[ix]))
            })
            .unwrap_or(1);
        let col = col.filter(|_| row_new_line(&data.rows[row]).is_some());
        let path = data
            .src
            .repo_root
            .join(data.diff.files[file].display_path());
        let target = match col {
            Some(col) => format!("{}:{line}:{col}", path.display()),
            None => format!("{}:{line}", path.display()),
        };
        match std::process::Command::new("zeditor").arg(&target).spawn() {
            // The CLI hands off to the running Zed and exits; reap it.
            Ok(mut child) => {
                std::thread::spawn(move || child.wait());
            }
            Err(err) => {
                self.refresh_error = Some(format!("couldn't run zeditor: {err}").into());
                cx.notify();
            }
        }
    }

    fn clear_viewed(&mut self, cx: &mut Context<Self>) {
        self.viewed.clear();
        self.apply_viewed(cx);
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
        let row = self
            .tree_row_file(rows[pos])
            .and_then(|file| self.file_landing_row(file));
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
            .text_size(px(18.))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    // Escape in the sidebar hands focus back to the diff; the
                    // filter stays applied (it also filters the diff pane).
                    .on_action(cx.listener(|this, _: &InputEscape, window, cx| {
                        this.creating_bucket = false;
                        window.focus(&this.focus_handle);
                        cx.notify();
                    }))
                    .child(div().p_2().child(Input::new(&self.tree_filter_input)))
                    .child(self.render_bucket_tabs(cx))
                    .when(
                        self.selected == buckets::Selected::Filtered && !self.exclude.is_empty(),
                        |col| {
                            col.child(div().px_2().pb_1().flex().flex_wrap().gap_1().children(
                                self.exclude.iter().cloned().enumerate().map(|(ix, pat)| {
                                    render_exclude_tag(ix, SharedString::from(pat), &entity)
                                }),
                            ))
                        },
                    )
                    .when(self.selected == buckets::Selected::Filtered, |col| {
                        col.child(div().px_2().pb_1().child(Input::new(&self.exclude_input)))
                    })
                    .when(!self.viewed.is_empty(), |col| {
                        let n = self.active_data().map_or(0, |data| data.viewed_count);
                        col.child(
                            div()
                                .id("viewed-clear")
                                .px_2()
                                .pb_1()
                                .text_size(px(16.))
                                .text_color(theme::overlay0())
                                .cursor_pointer()
                                .hover(|s| s.text_color(theme::text()))
                                .child(SharedString::from(format!("{n} viewed · show again")))
                                .on_click(cx.listener(|this, _, _, cx| this.clear_viewed(cx))),
                        )
                    })
                    .when(!self.review.comments.is_empty(), |col| {
                        let n = self.review.comments.len();
                        let link = |id: &'static str, label: SharedString| {
                            div()
                                .id(id)
                                .cursor_pointer()
                                .hover(|s| s.text_color(theme::text()))
                                .child(label)
                        };
                        col.child(
                            div()
                                .px_2()
                                .pb_1()
                                .flex()
                                .gap_2()
                                .text_size(px(16.))
                                .text_color(theme::overlay0())
                                .child(div().text_color(theme::peach()).child(SharedString::from(
                                    format!("{n} comment{}", if n == 1 { "" } else { "s" }),
                                )))
                                .child(
                                    link(
                                        "review-copy",
                                        if self.review_copied {
                                            "copied ✓"
                                        } else {
                                            "copy report (c)"
                                        }
                                        .into(),
                                    )
                                    .on_click(cx.listener(|this, _, _, cx| this.copy_review(cx))),
                                )
                                .child(
                                    link("review-clear", "clear (x)".into()).on_click(
                                        cx.listener(|this, _, _, cx| this.clear_review(cx)),
                                    ),
                                ),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .key_context("FileTree")
                            .track_focus(&self.tree_focus)
                            .on_action(
                                cx.listener(|this, _: &TreeUp, _, cx| this.tree_move(-1, cx)),
                            )
                            .on_action(
                                cx.listener(|this, _: &TreeDown, _, cx| this.tree_move(1, cx)),
                            )
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
            .child(self.render_commit_box(cx))
    }

    /// Message box and commit button pinned under the file tree.
    fn render_commit_box(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let empty = self.commit_input.read(cx).value().trim().is_empty();
        let n = self.bucket_commit_files(cx).len();
        let bucket = match &self.selected {
            buckets::Selected::All => "all".to_string(),
            buckets::Selected::Default => "Default".to_string(),
            buckets::Selected::Filtered => "filtered out".to_string(),
            buckets::Selected::Named(name) => name.clone(),
        };
        div()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .border_t_1()
            .border_color(theme::surface0())
            .on_action(cx.listener(|this, _: &InputEscape, window, cx| {
                window.focus(&this.focus_handle);
                cx.notify();
            }))
            .child(Input::new(&self.commit_input))
            .child(
                Button::new("commit")
                    .primary()
                    .w_full()
                    .label(if self.committing {
                        "committing…".to_string()
                    } else {
                        format!("Commit {bucket} ({n})")
                    })
                    .loading(self.committing)
                    .disabled(empty || self.committing || n == 0)
                    .on_click(cx.listener(|this, _, window, cx| this.commit(window, cx))),
            )
            .when_some(self.commit_status.clone(), |col, status| {
                let (text, color) = match status {
                    Ok(summary) => (
                        SharedString::from(format!("committed {summary}")),
                        theme::green(),
                    ),
                    Err(err) => (err, theme::red()),
                };
                col.child(div().text_size(px(15.)).text_color(color).child(text))
            })
    }

    fn render_sidebar_resizer(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .w(px(4.))
            .flex_shrink_0()
            .h_full()
            .cursor_col_resize()
            .bg(theme::surface0())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    this.sidebar_resizing = true;
                    this.sidebar_resize_start =
                        Some((f32::from(event.position.x), this.sidebar_width));
                    cx.stop_propagation();
                }),
            )
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
            ("left / right", "scroll sideways"),
            ("v", "unified / split"),
            ("/", "filter files"),
            ("ctrl-f", "filter files"),
            ("escape", "switch file tree / diff"),
            ("space", "mark file / folder viewed (hide it)"),
            ("z", "open in Zed at this line"),
            ("home", "top"),
            ("end", "bottom"),
            ("ctrl-b", "toggle sidebar"),
            ("r", "refresh"),
            ("ctrl-=", "bigger font"),
            ("ctrl--", "smaller font"),
            ("ctrl-0", "reset font"),
            ("ctrl-c", "copy selection"),
            ("enter", "comment on hunk / selected lines"),
            ("c", "copy review report"),
            ("x", "clear review report"),
            ("m", "move file / folder to a bucket"),
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
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    this.keybindings_visible = false;
                    cx.notify();
                }),
            )
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
                // Thin outline around the cursor's hunk, drawn over the list
                // so it stays put while the text scrolls sideways.
                let hunk_outline = data.current_hunk().map(|(_, start, end)| {
                    let rh = row_height();
                    let top = start as f32 * rh
                        + f32::from(data.scroll.0.borrow().base_handle.offset().y);
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .top(px(top))
                        .h(px((end - start) as f32 * rh))
                        .border_1()
                        .border_color(theme::blue())
                });
                div()
                    .size_full()
                    .relative()
                    .overflow_hidden()
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
                                    let hunk =
                                        data.current_hunk().map(|(_, start, end)| start..end);
                                    range
                                        .filter_map(|ix| data.rows.get(ix).map(|row| (ix, row)))
                                        .map(|(ix, row)| {
                                            let row_sel = sel.and_then(|sel| {
                                                row_selection_range(&sel, ix, row)
                                                    .filter(|range| !range.is_empty())
                                                    .map(|range| (sel.side, range))
                                            });
                                            let el = match row {
                                                Row::HunkHeader { label } => {
                                                    let end = data
                                                        .hunk_rows
                                                        .iter()
                                                        .chain(&data.file_rows)
                                                        .copied()
                                                        .filter(|&r| r > ix)
                                                        .min()
                                                        .unwrap_or(data.rows.len());
                                                    let note = data
                                                        .comment_target(ix..end, None, true)
                                                        .and_then(|(path, span, _)| {
                                                            this.review.find(&path, &span)
                                                        })
                                                        .map(|c| {
                                                            this.review.comments[c].body.as_str()
                                                        });
                                                    render_hunk_header(label, note)
                                                }
                                                _ => render_row(row, row_sel, split_x),
                                            };
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
                    .children(hunk_outline)
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
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, _| {
                    this.sidebar_resizing = false;
                    this.sidebar_resize_start = None;
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, _| {
                    this.sidebar_resizing = false;
                    this.sidebar_resize_start = None;
                }),
            )
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
            .on_action(
                cx.listener(|this, _: &OpenInEditor, window, cx| this.open_in_editor(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &MarkViewed, window, cx| this.mark_viewed(None, window, cx)),
            )
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
            .on_action(
                cx.listener(|this, _: &EditComment, window, cx| this.open_comment(window, cx)),
            )
            .on_action(cx.listener(|this, _: &CopyReview, _, cx| this.copy_review(cx)))
            .on_action(cx.listener(|this, _: &ClearReview, _, cx| this.clear_review(cx)))
            .on_action(
                cx.listener(|this, _: &MoveToBucket, window, cx| {
                    this.open_bucket_picker(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &ScrollLeft, window, cx| {
                this.scroll_x_by_key(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ScrollRight, window, cx| {
                this.scroll_x_by_key(true, window, cx)
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
                        // The comment editor sits outside the ReviewApp key
                        // context so typing in it doesn't hit diff bindings.
                        div()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .relative()
                            .child(
                                div()
                                    .size_full()
                                    .key_context("ReviewApp")
                                    .track_focus(&self.focus_handle)
                                    .child(pane),
                            )
                            .children(self.render_comment_editor(cx)),
                    ),
            )
            .when(self.keybindings_visible, |root| {
                root.child(self.render_keybindings(cx))
            })
            .children(self.render_bucket_picker(cx))
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Default to the repo we're standing in; a single directory argument is
    // also accepted. Anything else is ignored (we only review local diffs).
    let repo_path = args
        .first()
        .filter(|arg| Path::new(arg).is_dir())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    Application::new()
        .with_assets(gpui_component_assets::Assets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            theme::init(cx);
            theme::watch_omarchy_theme(cx);
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
                KeyBinding::new("left", ScrollLeft, Some("ReviewApp")),
                KeyBinding::new("right", ScrollRight, Some("ReviewApp")),
                KeyBinding::new("right", TreeExpand, Some("FileTree")),
                KeyBinding::new("escape", FocusDiff, Some("FileTree")),
                KeyBinding::new("space", MarkViewed, Some("FileTree")),
                KeyBinding::new("space", MarkViewed, Some("ReviewApp")),
                KeyBinding::new("z", OpenInEditor, Some("ReviewApp")),
                KeyBinding::new("z", OpenInEditor, Some("FileTree")),
                KeyBinding::new("escape", ClearSelection, Some("ReviewApp")),
                KeyBinding::new("ctrl-c", CopySelection, Some("ReviewApp")),
                KeyBinding::new("enter", EditComment, Some("ReviewApp")),
                KeyBinding::new("c", CopyReview, Some("ReviewApp")),
                KeyBinding::new("x", ClearReview, Some("ReviewApp")),
                KeyBinding::new("m", MoveToBucket, Some("ReviewApp")),
                KeyBinding::new("m", MoveToBucket, Some("FileTree")),
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
                    // Wayland app_id; matches omarchy/agent-review.desktop so
                    // the compositor finds the launcher's icon.
                    app_id: Some("agent-review".into()),
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
    use super::*;

    fn item_with_hidden(hidden: &[usize]) -> ItemData {
        let patch = ["a/x.txt", "z/w.txt", "z/y.txt"]
            .iter()
            .map(|p| {
                format!("diff --git a/{p} b/{p}\n--- a/{p}\n+++ b/{p}\n@@ -1 +1,2 @@\n one\n+two\n")
            })
            .collect::<String>();
        let diff = diff_core::parse_patch(&patch);
        let hidden: HashSet<usize> = hidden.iter().copied().collect();
        let (rows, file_rows, hunk_rows) = build_rows(
            &diff,
            ViewMode::Unified,
            shown_files(&diff, "", &hidden).as_ref(),
        );
        let (widest_row_ix, max_line_chars) = widest_line(&rows);
        ItemData {
            src: git::LocalSource {
                repo_root: PathBuf::from("/r"),
                branch: "main".into(),
                base_ref: None,
                base_label: "HEAD".into(),
                base_oid: None,
            },
            diff,
            mode: ViewMode::Unified,
            rows,
            file_rows,
            hunk_rows,
            max_line_chars,
            widest_row_ix,
            split_scroll_x: 0.,
            cursor: 0,
            scroll: UniformListScrollHandle::new(),
            additions: 0,
            deletions: 0,
            selection: None,
            tree: Vec::new(),
            collapsed: HashSet::new(),
            tree_scroll: UniformListScrollHandle::new(),
            tree_last_file: None,
            hidden,
            viewed_count: 0,
            states: HashMap::new(),
        }
    }

    /// Hiding the trailing files leaves their `file_rows` entries at
    /// `rows.len()`; path lookups must skip them rather than index past the end.
    #[test]
    fn lookups_survive_hidden_trailing_files() {
        let data = item_with_hidden(&[1, 2]);
        assert!(data.file_rows.iter().any(|&f| f >= data.rows.len()));
        let span = review::Span {
            side: review::Side::New,
            start: 2,
            end: 2,
        };
        assert!(data.span_end_row("z/y.txt", &span).is_none());
        let row = data.span_end_row("a/x.txt", &span).unwrap();
        let anchor = data.anchor_at(row).unwrap();
        assert_eq!(data.resolve_anchor(&anchor), Some(row));
        let gone = RowAnchor {
            path: "z/w.txt".into(),
            lines: None,
            from_file: 0,
        };
        assert_eq!(data.resolve_anchor(&gone), None);
    }

    #[test]
    fn hidden_files_follow_the_selected_bucket() {
        use buckets::{Buckets, Selected};
        let data = item_with_hidden(&[]);
        let mut b = Buckets::default();
        b.create("one");
        b.assign(["z/w.txt"], Some("one"));
        let viewed = HashMap::from([("a/x.txt".to_string(), file_hash(&data.diff.files[0]))]);
        let (hidden, n) = hidden_files(&data.diff, &viewed, &b, &[], &Selected::All);
        assert_eq!((hidden, n), (HashSet::from([0]), 1));
        let (hidden, n) =
            hidden_files(&data.diff, &viewed, &b, &[], &Selected::Named("one".into()));
        assert_eq!((hidden, n), (HashSet::from([0, 2]), 0));
        let (hidden, n) = hidden_files(&data.diff, &viewed, &b, &[], &Selected::Default);
        assert_eq!((hidden, n), (HashSet::from([0, 1]), 1));
        // A pattern match puts a/x.txt in Filtered out and nowhere else;
        // viewed still hides it there.
        let filters = vec!["a/*".to_string()];
        let none = HashMap::new();
        let (hidden, _) = hidden_files(&data.diff, &none, &b, &filters, &Selected::Filtered);
        assert_eq!(hidden, HashSet::from([1, 2]));
        let (hidden, n) = hidden_files(&data.diff, &viewed, &b, &filters, &Selected::Filtered);
        assert_eq!((hidden, n), (HashSet::from([0, 1, 2]), 1));
        let (hidden, n) = hidden_files(&data.diff, &none, &b, &filters, &Selected::All);
        assert_eq!((hidden, n), (HashSet::from([0]), 0));
    }

    /// A hidden file in the middle shares the next file's header row.
    #[test]
    fn lookups_with_hidden_middle_file() {
        let data = item_with_hidden(&[1]);
        let span = review::Span {
            side: review::Side::New,
            start: 2,
            end: 2,
        };
        let row = data.span_end_row("z/y.txt", &span).unwrap();
        let (path, range) = data.file_of(row).unwrap();
        assert_eq!(path.as_ref(), "z/y.txt");
        assert!(range.contains(&row));
        let (_, span_found, _) = data.comment_target(range, None, true).unwrap();
        assert_eq!(span_found, span);
    }

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
                vec![hunk(vec![
                    rem(1, "a"),
                    rem(2, "b"),
                    add(1, "c"),
                    add(2, "d"),
                ])],
            )],
        };
        let (rows, _, _) = build_rows(&diff, ViewMode::Split, None);
        assert_eq!(rows.len(), 4);
        assert!(matches!(
            &rows[2],
            Row::SplitLine {
                left: Some(_),
                right: Some(_)
            }
        ));
        assert!(matches!(
            &rows[3],
            Row::SplitLine {
                left: Some(_),
                right: Some(_)
            }
        ));
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
