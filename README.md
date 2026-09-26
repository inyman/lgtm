# lgtm

A minimal, native local git-diff viewer in Rust, built with [gpui](https://www.gpui.rs/).

Open it inside a repository (or pass a path) and it shows everything not yet
committed — staged, unstaged, and untracked — as one reviewable diff. Comment on
hunks, copy the review back to the agent, commit. No accounts, no network.

## Why

When a local agent (Claude Code, a coding assistant, a script, whatever) works in
your repo, the first question is always the same: *what did it actually change?*
The answer is the git diff — but reading raw unified diff text in a terminal is
no way to review anything beyond a handful of files.

This is a viewer for exactly that moment. It was rebuilt from
[ellie/lgtm](https://github.com/ellie/lgtm) (the PR-review app) with everything
stripped away except the local diff: no GitHub, no auth, no review comments, no
chat. Run it where the agent just worked, and you get a proper, review-grade look
at the working tree — syntax-highlighted, word-level diffs, file tree — before
you commit or throw it away.

## Setup

```sh
cargo run --release
```

Or open a specific repository:

```sh
cargo run --release -- /path/to/repo
```

> On macOS you need Xcode to build gpui from source (it bundles the metal dev
> tools). Otherwise the build fails with:
> ```
> cargo::error=metal shader compilation failed:
> xcrun: error: unable to find utility "metal", not a developer tool or in PATH
> ```

## Features
- unified + split views
- tree-sitter syntax highlighting
- word-level intra-line diffs
- resizable sidebar with file tree + fuzzy filter
- exclude patterns (e.g. `__generated__`, `*.png`) and mark-as-viewed to hide files
- live reload as files change, keeping your scroll position
- review comments on hunks or selected lines, copied as a report to paste back to the agent
- commit box: stage everything and commit without leaving the app
- mouse selection + copy
- colors follow the active [Omarchy](https://omarchy.org) theme when present, else Catppuccin Mocha

## Reviewing

Press `enter` on a hunk (or on a mouse selection) to comment on it; `esc` saves,
an empty comment deletes. Commented hunks show the note in their `@@` header.
`c` copies the review:

````markdown
1. src/main.rs:190-200

```diff
-old line
+new line
```

this should be like this or that
````

Line numbers are the working-tree file's (`path:start-end`); spans covering only
removed lines are marked as base-version numbering. The review is kept in
`<git dir>/lgtm/review.json` (plus a `review.md` copy), so it survives restarts
and never shows up in the diff. Committing clears it.

## Keymap
| Key | Action |
|---|---|
| `]` / `[` | next / previous file |
| `n` / `p` | next / previous hunk |
| `down` / `up` | scroll through the hunk, then next / previous hunk |
| `left` / `right` | scroll sideways |
| `home` / `end` | top / bottom |
| `v` | unified ↔ split view |
| `enter` | comment on hunk / selected lines |
| `c` | copy review report |
| `x` | clear review report |
| `space` | mark file (or folder, in the tree) viewed — hides it until it changes |
| `z` | open in Zed at this line (`zeditor`) |
| `/` or `ctrl-f` | focus file filter |
| `esc` | clear selection / switch between diff and file tree |
| `r` | refresh |
| `ctrl-b` | toggle sidebar |
| `ctrl-+` / `ctrl--` / `ctrl-0` | diff font size: bigger / smaller / reset |
| `ctrl-c` | copy selection |
| `ctrl-enter` | commit (in the commit box) |
| `ctrl-k` | show keybindings |
| `ctrl-q` | quit |

In the file tree: `up` / `down` move, `enter` opens a file or folds a folder,
`left` / `right` collapse / expand.
