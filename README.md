# Glauca

A keyboard-driven GitHub issue & pull request inbox manager.

## Overview

Glauca lets you save GitHub search queries (for example `is:pr review-requested:@me`),
organize them into filter streams, and keep them in one place. It syncs matching issues and
pull requests into a local SQLite cache in the background, tracks what you've read, shows
unread counts, and can notify you when new items arrive — so you can triage everything that
needs your attention without juggling browser tabs.

It ships as two frontends sharing the same core: a terminal UI and a desktop GUI.

## Features

- **Saved queries & filter streams** — store GitHub search queries and split them into
  sub-filters for finer organization.
- **Local cache** — items are stored in SQLite, so browsing stays fast and works offline.
- **Background auto-sync** — configurable interval (default 60s) with automatic backoff on
  rate limits.
- **Desktop notifications** — optional OS-level notifications for new/updated items (toggleable).
- **Markdown rendering** — read issue and PR bodies with formatting.
- **Item actions** — comment, approve, and merge pull requests from within the app.
- **Custom actions** — run your own commands (or `gh`) against the selected item with
  templated arguments (`x`); see [Custom actions](#custom-actions).
- **Read tracking** — per-item read status and unread counts per query.
- **Keyboard-driven** — navigate entirely from the keyboard.

## Architecture

Glauca is a Cargo workspace of four crates:

| Crate | Role |
| --- | --- |
| `glauca-core` | Shared library: database, GitHub API integration, sync engine |
| `glauca-tui` | Terminal UI (built with `ratatui`) |
| `glauca-gui` | Desktop GUI (built with `gpui`) |
| `glauca-tauri` | Desktop web-tech UI (built with `tauri`, plain HTML/CSS/JS) |

Every front-end is a thin shell over `glauca-core`'s async engine, talking to it
through the same `EngineCommand` / `AppMessage` message protocol.

The GitHub client uses `octocrab` plus GraphQL search; the cache uses `sqlx` with SQLite.

## Requirements

- **Rust** (edition 2024). Building the GUI requires **rustc ≥ 1.95** (a dependency of `gpui`).
- A **GitHub token** (see [Configuration](#configuration)).
- For the **GUI on Linux**: X11 or Wayland and a working GPU.

> Note: the GUI depends on `gpui` crates pulled directly from git, so the first build downloads
> and compiles them.

## Installation

### Via gh extension (TUI)

The terminal UI is distributed as a [`gh` CLI](https://cli.github.com/) extension:

```bash
gh extension install okkez/gh-glauca
gh glauca
```

### From source

```bash
git clone https://github.com/okkez/glauca.git
cd glauca

# Build the workspace
DATABASE_URL="sqlite:crates/glauca-core/dev.db" cargo build --release
```

## Usage

```bash
# Terminal UI
cargo run -p glauca-tui

# Desktop GUI (requires X11/Wayland + GPU). Use --release for daily use —
# dev builds trade some runtime speed for compile speed.
cargo run --release -p glauca-gui

# Desktop web-tech UI (Tauri). Needs the Tauri CLI (Rust, no Node):
#   cargo install tauri-cli
cargo tauri dev --config crates/glauca-tauri/tauri.conf.json
# or just build/run the binary directly:
cargo run -p glauca-tauri
```

> The `glauca-tauri` front-end uses a build-step-free, framework-free static
> front-end (`crates/glauca-tauri/ui/`) and the system WebView (WebKitGTK on
> Linux, WKWebView on macOS, WebView2 on Windows). No Node toolchain is involved.
> It mirrors the TUI/GUI feature set — browse/filter/sync/read, item actions
> (comment/review/merge), query & filter-stream editing, custom actions, and
> the TUI keymap (press `?` for the reference). Markdown bodies render as plain
> text for now; the octorus review integration stays TUI-only.

Glauca authenticates to GitHub using a personal access token from your environment. The
easiest way is to install the [`gh` CLI](https://cli.github.com/) and run `gh auth login`,
which sets `GH_TOKEN` for you.

## Filtering: queries vs. filter streams

Glauca has two distinct filtering layers:

- **Saved queries** are sent verbatim to the GitHub search API, so any
  [GitHub search qualifier](https://docs.github.com/en/search-github/searching-on-github/searching-issues-and-pull-requests)
  works (`created:`, `language:`, `involves:`, `linked:`, …).
- **Filter streams** (and the inline filter) run locally against the cached items, so they
  only understand the subset of qualifiers below. Unknown qualifiers match nothing.

Supported local filter qualifiers (all conditions are ANDed; matching is a
case-insensitive substring unless noted):

| Qualifier | Matches |
| --- | --- |
| _plain text_ | title, author, or label |
| `is:pr` / `is:issue` | item kind (exact) |
| `is:open` / `is:closed` / `is:merged`, `state:<v>` | state |
| `is:draft` | draft pull requests only |
| `is:public` / `is:private` | repository visibility |
| `author:<login>` | author login |
| `assignee:<login>` | an assignee login |
| `label:<name>` | a label |
| `milestone:<title>` | milestone title (substring; value cannot contain spaces) |
| `repo:<owner/name>` | repository |
| `base:<branch>` / `head:<branch>` | PR base / head branch |
| `review-requested:<login>` | a requested reviewer login |

`@me` is expanded to the current user in both layers.

### Servo rendering (experimental)

`glauca-tauri` can, in principle, render through **Servo** instead of the system
WebView, using [`tauri-runtime-verso`](https://github.com/tauri-apps/tauri-runtime-verso)
(Tauri's Servo/Verso backend). This is **experimental and opt-in**, and is *not*
wired in by default because:

- `tauri-runtime-verso` is not published on crates.io and its API is in flux.
- It requires a separately built `versoview` executable.
- IPC / multi-window support is partial.

Because the front-end is plain HTML/CSS/JS (no heavy framework runtime), it is a
good fit for Servo's current capabilities. To experiment:

1. Build `versoview` from the Verso project.
2. Add `tauri-runtime-verso` (git dependency) to `crates/glauca-tauri/Cargo.toml`.
3. Switch the builder in `crates/glauca-tauri/src/main.rs` to
   `tauri::Builder::<VersoRuntime>::new()…`, point it at the `versoview` binary,
   and add `.invoke_system(INVOKE_SYSTEM_SCRIPTS.to_owned())`.

The default `cargo run -p glauca-tauri` keeps using the stable system WebView.


## Configuration

### Environment variables

| Variable | Purpose |
| --- | --- |
| `GH_TOKEN` | GitHub token (checked first; set automatically by the `gh` CLI) |
| `GITHUB_TOKEN` | GitHub token (fallback) |
| `RUST_LOG` | Log level, e.g. `glauca_core=debug` |
| `DATABASE_URL` | Override the database path (development only) |

### Settings files

Each frontend keeps its own TOML settings under the config directory. A missing or invalid
file falls back to defaults.

The GUI reads `~/.config/glauca/gui.toml`:

```toml
pane_sizes = [200.0, 600.0, 400.0]  # left / center / right pane widths
theme = "system"                     # "system" | "light" | "dark"
notifications_enabled = false        # toggle desktop notifications
sync_interval_secs = 60              # background sync interval, in seconds
```

The TUI reads `~/.config/glauca/tui.toml`:

```toml
notifications_enabled = false        # toggle desktop notifications
sync_interval_secs = 60              # background sync interval, in seconds
```

### Custom actions

Both frontends can run user-defined commands against the selected PR/Issue. Press `x`
on an item to open a picker of the actions that apply to it, then choose one to run.

Actions are shared by both frontends and defined in `~/.config/glauca/actions.toml`:

```toml
[[actions]]
name = "review"                       # identifier (also the fallback label)
label = "AI review"                   # optional display label
command = ["my-review-script", "{{ repo_full }}", "{{ number }}"]
kinds = ["pull_request"]              # optional; empty/omitted = every kind

[[actions]]
name = "checkout"
label = "gh pr checkout"
command = ["gh", "pr", "checkout", "{{ number }}", "-R", "{{ repo_full }}"]
kinds = ["pull_request"]
```

Each `command` is an argv list (the first element is the program). It is run directly —
no shell — so `gh` and your own scripts are invoked as-is and inherit the environment.
Every element is a template: `{{ key }}` placeholders are substituted from the selected
item before the command runs. An unknown key is an error (surfacing typos rather than
running a wrong command). An optional `env` table sets extra environment variables, whose
values are templated the same way.

Available template variables (all strings):

`owner`, `repo`, `repo_full` (`owner/repo`), `number`, `kind`, `url`, `title`, `author`,
`state`, `is_draft`, `base_ref`, `head_ref`, `created_at`, `updated_at`.

Output is not captured; the action runs in the background and reports success or failure
(with stderr on failure) via the status line and desktop notifications.

### Cache location

Cached items are stored at `~/.local/share/glauca/cache.db` (created automatically).

## Development

```bash
# Run the test suite
DATABASE_URL="sqlite:crates/glauca-core/dev.db" cargo test
```

`sqlx` checks queries at compile time, so `DATABASE_URL` must point at a SQLite database when
building and testing.

## License

Released under the [MIT License](LICENSE).
