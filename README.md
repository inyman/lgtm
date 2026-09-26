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
- a Filtered out bucket for noise (e.g. `__generated__`, `*.png`) and mark-as-viewed to hide files
- live reload as files change, keeping your scroll position
- review comments on hunks or selected lines, copied as a report to paste back to the agent
- buckets: sort changed files into named groups and commit one group at a time
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

Tabs under the file filter show **All**, **Default** (files not sorted yet), your
own buckets, and **Filtered out**; `+` adds a bucket. A **●** marks a bucketed file
(and its tab) that changed since you sorted it; marking it viewed clears it. `m` moves the current file —
or the file/folder under the tree cursor — to a bucket.

**Filtered out** holds every file matching its patterns (`__generated__`, `*.wasm`,
`*.glb`, `*.png` by default; select the tab to add or remove them). A pattern
match wins over any bucket, and **All** means everything except Filtered out —
select the tab to review or commit those files on their own. The selected tab filters the tree and the
diff, and the commit box commits exactly that bucket's files. Buckets are just a
view: they live in `<git dir>/lgtm/buckets.json`, and nothing is moved or staged.

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
