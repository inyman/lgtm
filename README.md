# lgtm

A minimal, native local git-diff viewer in Rust, built with [gpui](https://www.gpui.rs/).

Open it inside a repository (or pass a path) and it shows everything that changed
since the diff base — committed, staged, unstaged, and untracked — as one
reviewable diff. That's it. No accounts, no network.

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
- mouse selection + copy

## Keymap
| Key | Action |
|---|---|
| `]` / `[` | next / previous file |
| `n` / `p` | next / previous hunk |
| `v` | unified ↔ split view |
| `/` | focus file filter |
| `home` / `end` | top / bottom |
| `ctrl-b` | toggle sidebar |
| `r` | refresh |
| `ctrl-+` / `ctrl--` / `ctrl-0` | diff font size: bigger / smaller / reset |
| `ctrl-c` | copy selection |
| `ctrl-k` | show keybindings |
| `ctrl-q` | quit |
