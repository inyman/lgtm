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
- live reload as files change, keeping your scroll position (git-ignored trees are
  never watched — a `node_modules` / `target` is thousands of directories the diff
  can't show, and arming them froze the window for seconds)
- review comments on hunks or selected lines, copied as a report to paste back to the agent
- buckets: path patterns (`docs/context`, `*.glb`, `__generated__`) sort changed files into
  named groups you commit one group at a time; mark-as-viewed moves a file out of the way
- `z` opens the file in [Zed](https://zed.dev) at the line you're on
- mouse selection + copy
- colors follow the active [Omarchy](https://omarchy.org) theme when present, else Catppuccin Mocha

## Reviewing

Press `enter` on a hunk, a line (`shift-down` / `shift-up`) or a mouse selection to comment on it; `esc` saves,
an empty comment deletes. Commented hunks show the note in their `@@` header.
`c` copies the review for the selected bucket's files (`x` clears them):

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
and never shows up in the diff. Committing drops the comments on the committed
files.

## Buckets and committing

Tabs under the file filter show **Unsorted** (files no bucket claims), your
buckets, and **Viewed**; `+` adds a bucket. The selected tab filters the tree and
the diff, and the commit box commits exactly that tab's files.

A bucket claims files two ways:

- **path patterns** — select the bucket, type a pattern under the tabs, `enter`
  pins it (it applies while you type). A pattern matches any run of whole path
  segments: `docs/context` takes everything under that folder, `*.glb` matches by
  file name, `__generated__` any folder of that name. Each pattern shows as a tag
  with a `×` to remove it.
- **by hand** — `m` moves the current file, or the file/folder under the tree
  cursor, into a bucket.

Every file sits in exactly one place: its hand assignment, else the first bucket
(left to right) whose pattern matches, else Unsorted. A **●** marks a hand-sorted
file (and its tab) that changed since you sorted it; marking it viewed clears it.

`space` marks a file viewed: it leaves its bucket for **Viewed** until its diff
changes. In Viewed, `space` sends it back, and Commit commits the viewed files.

Buckets are just a view — nothing is moved or staged — and a repo starts with
none. Their names, order and patterns live in `<common git dir>/lgtm/bucket-defs.json`,
shared by every worktree of the repo; hand assignments are per worktree, in
`<git dir>/lgtm/bucket-files.json`.

Committing never gets in the way of an agent working in the repo, or of your own
staging:

- the commit is built in a private index under `<git dir>/lgtm/`, never in git's
  staging area; files outside the bucket — staged or not — are left as they are
- each file must still be exactly what you reviewed; if the agent changed it
  since, nothing is committed and the view refreshes
- HEAD only moves if no other commit landed meanwhile
- afterwards, git's staging area is updated for the committed files only, as
  `git commit -- <files>` would
- hooks don't run
- all reads run with `GIT_OPTIONAL_LOCKS=0`, so viewing never takes git's index
  lock

## Zed

`z` hands the spot you're looking at to [Zed](https://zed.dev), so you can go
from reviewing to editing in one key:

- on a line (`shift-down` / `shift-up`, or a click) it opens that line
- on a mouse selection it opens at the selection, column included
- on a hunk it opens the hunk's first line; in the file tree, the file's first
  hunk
- a removed line no longer exists in the file, so it opens the nearest line
  that does

It runs `zeditor <file>:<line>[:<col>]`, which opens the file in the running
Zed. That's the CLI's name on Linux; make sure it's on your `PATH` (on macOS
Zed's CLI is `zed`, which lgtm doesn't call yet).

## Keymap
| Key | Action |
|---|---|
| `]` / `[` | next / previous file |
| `n` / `p` | next / previous hunk |
| `down` / `up` | scroll through the hunk, then next / previous hunk |
| `shift-down` / `shift-up` | next / previous changed line (line cursor for `z` and `enter`; a click sets it too) |
| `left` / `right` | scroll sideways |
| `home` / `end` | top / bottom |
| `v` | unified ↔ split view |
| `enter` | comment on hunk / selected lines |
| `c` | copy review report (selected bucket's files) |
| `x` | clear review report (selected bucket's files) |
| `m` | move file / folder to a bucket (`1`–`9` pick, `n` new) |
| `space` | mark file (or folder, in the tree) viewed — it moves to Viewed until it changes; in Viewed, back to its bucket |
| `z` | open in Zed at this line (`zeditor`) |
| `/` or `ctrl-f` | focus file filter |
| `tab` | diff ⇄ file tree |
| `esc` | back to the diff from anywhere; in the diff, clear the selection / line cursor |
| `r` | refresh |
| `ctrl-b` | toggle sidebar |
| `ctrl-+` / `ctrl--` / `ctrl-0` | diff font size: bigger / smaller / reset |
| `ctrl-c` | copy selection |
| `ctrl-enter` | commit (in the commit box) |
| `ctrl-k` | show keybindings |
| `ctrl-q` | quit |

In the file tree: `up` / `down` move, `enter` opens a file or folds a folder,
`left` / `right` collapse / expand.
