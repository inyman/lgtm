# lgtm

A minimal, native local git-diff viewer in Rust, built with [gpui](https://www.gpui.rs/).

Open it inside a repository (or pass a path) and it shows everything that changed
since the diff base — committed, staged, unstaged, and untracked — as one
reviewable diff. That's it. No accounts, no network.

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
- sidebar file tree with fuzzy filter
- minimap
- mouse selection + copy

## Keymap
| Key | Action |
|---|---|
| `]` / `[` | next / previous file |
| `n` / `p` | next / previous hunk |
| `v` | unified ↔ split view |
| `m` | toggle minimap |
| `/` | focus file filter |
| `home` / `end` | top / bottom |
| `cmd-b` | toggle sidebar |
| `r` | refresh |
| `cmd-+` / `cmd--` / `cmd-0` | diff font size: bigger / smaller / reset |
| `cmd-c` | copy selection |
| `cmd-q` | quit |
