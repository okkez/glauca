# Glauca

A keyboard-driven GitHub issue & pull request inbox manager.

## Overview

Glauca lets you save GitHub search queries (for example `is:pr review-requested:@me`),
organize them into filter streams, and keep them in one place. It syncs matching issues and
pull requests into a local SQLite cache in the background, tracks what you've read, shows
unread counts, and can notify you when new items arrive — so you can triage everything that
needs your attention without juggling browser tabs.

It ships as three frontends sharing the same core: a terminal UI, a native desktop GUI,
and a web-tech desktop app (Tauri).

## Features

- **Saved queries & filter streams** — store GitHub search queries and split them into
  sub-filters for finer organization.
- **Local filtering** — narrow cached items with an inline filter, including `-` prefix
  negation (e.g. `-is:draft`, `-label:bug`); see [Filtering](#filtering-queries-vs-filter-streams).
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
- For the **Tauri app**: the Tauri CLI (`cargo install tauri-cli`) and a system WebView
  (WebKitGTK on Linux, WKWebView on macOS, WebView2 on Windows). No Node toolchain is needed.

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
  only understand the subset of qualifiers below. An unrecognized qualifier is not an
  error: it is treated as plain text and fuzzy-matched like any other search term, so
  `nonsense:value` usually matches nothing but is not guaranteed to.

Supported local filter qualifiers (all conditions are ANDed; matching is a
case-insensitive substring unless noted):

| Qualifier | Matches |
| --- | --- |
| _plain text_ | title, author, or label |
| `-<token>` | negates any token below (`-label:bug`, `-is:draft`, `-wip`) — the item must not match it |
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
| `team-review-requested:<slug>` | a requested reviewer team. Teams and users share one list locally, so this and `review-requested:` are interchangeable (unlike on GitHub) |

`@me` is expanded to the current user in both layers.

### Servo rendering (not currently viable)

`glauca-tauri` renders through the system WebView (WebKitGTK / WKWebView /
WebView2). Rendering through **Servo** instead has been investigated, but as of
July 2026 there is no maintained bridge between Tauri and Servo:

- [`tauri-runtime-verso`](https://github.com/versotile-org/tauri-runtime-verso)
  (the experimental Tauri backend for Servo/Verso) is dormant — no commits since
  October 2025, never published to crates.io, and its
  [Verso](https://gitlab.com/verso-browser/verso) upstream stopped in mid-2025.
- Servo itself is healthy: it ships monthly as the
  [`servo` crate](https://crates.io/crates/servo) (plus an LTS track) since
  April 2026. But embedding it directly bypasses Tauri entirely — the IPC
  (`invoke`/`listen`), window management, and bundling would all need
  reimplementing, which amounts to writing a fourth frontend.

The front-end is plain HTML/CSS/JS (no heavy framework runtime), so nothing on
the rendering side blocks a future switch. Revisit if Tauri grows official
Servo support (watch the [Servo blog](https://servo.org/blog/) and
[tauri-apps discussions](https://github.com/orgs/tauri-apps/discussions/15235)).


## Configuration

### Environment variables

| Variable | Purpose |
| --- | --- |
| `GH_TOKEN` | GitHub token (checked first; set automatically by the `gh` CLI) |
| `GITHUB_TOKEN` | GitHub token (fallback) |
| `RUST_LOG` | Log level, e.g. `glauca_core=debug` |
| `GLAUCA_DB_PATH` | Cache database path for all front-ends, overriding the default. `--db-path` overrides it (see [Cache location](#cache-location)) |
| `DATABASE_URL` | `sqlx` compile-time query verification only — it has no effect on the database used at runtime (see [Development](#development)) |

### Settings files

Each frontend keeps its own TOML settings under the config directory. A missing or invalid
file falls back to defaults.

The GUI reads `~/.config/glauca/gui.toml`:

```toml
pane_sizes = [200.0, 600.0, 400.0]  # left / center / right pane widths
theme = "system"                     # "system" | "light" | "dark"
notifications_enabled = false        # toggle desktop notifications
sync_interval_secs = 60              # background sync interval, in seconds
full_fetch_interval_secs = 1800      # how often a sync re-fetches in full (see below)
```

The TUI reads `~/.config/glauca/tui.toml`:

```toml
notifications_enabled = false        # toggle desktop notifications
sync_interval_secs = 60              # background sync interval, in seconds
full_fetch_interval_secs = 1800      # how often a sync re-fetches in full (see below)
```

A background sync normally fetches only what changed since the last one, which is
cheap but cannot notice an item *leaving* a query (a PR that merges out of an
`is:open` query, or stops matching `team-review-requested:` once someone reviews
it). Every `full_fetch_interval_secs` a sync re-fetches the whole result set
instead and drops such items from the cache. Lower it to have them disappear
sooner; raise it to spend less API quota on queries with many results. For a query
whose results fit one page this costs no extra requests at all.

An item has to be missing from **two** such fetches in a row before it is dropped,
so expect it to disappear within two intervals rather than one. One absence isn't
proof: a paged fetch can race an update that moves an item out from under the
cursor, and GitHub's search index sometimes lags writes — deleting on the first
miss would throw away that item's read state for nothing. Two cases skip the wait,
because there the cached rows are known-stale rather than possibly-transient: a
full resync you ask for explicitly (`S`), and the first sync after you edit a
query's search string. Both accept the trade above — if such a fetch happens to
race an update, an item can be dropped and come back unread on the next sync.
(Renaming a query doesn't count as an edit here; the results are unchanged.)

Read/unread state lives on the cached row, so an item that leaves a query and later
matches it again comes back marked unread — a re-requested review or a reopened
issue reappears as new work rather than as something you had already read.

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
The database runs in WAL mode, so `cache.db-wal` and `cache.db-shm` appear alongside
it; delete all three together if you ever want to reset the cache.

All three front-ends share that one database. To point them somewhere else — a scratch
cache while developing, say — every front-end takes `--db-path`, and `GLAUCA_DB_PATH`
sets it for all of them at once. Missing parent directories are created.

```bash
cargo run -p glauca-tui -- --db-path /tmp/glauca-scratch/cache.db
cargo run --release -p glauca-gui -- --db-path /tmp/glauca-scratch/cache.db
GLAUCA_DB_PATH=/tmp/glauca-scratch/cache.db cargo run -p glauca-tauri

# Through the Tauri CLI the flag needs two separators, one per wrapper:
cargo tauri dev --config crates/glauca-tauri/tauri.conf.json -- -- --db-path /tmp/x.db
```

`--db-path` wins over `GLAUCA_DB_PATH`, which wins over the default. The path has to be a
new file or an existing glauca cache — a SQLite database glauca did not create is reported
instead of migrated, leaving its tables and rows as they were, and a cache written by a
newer glauca is rejected rather than downgraded.

Neither value goes through a shell, so a literal `~` is not expanded — write
`$HOME/cache.db` (or an absolute path) where no shell is involved, such as a systemd
unit's `Environment=`.

## Development

```bash
# Run the test suite
DATABASE_URL="sqlite:crates/glauca-core/dev.db" cargo test
```

`sqlx` checks queries at compile time, so `DATABASE_URL` must point at a SQLite database when
building and testing. It is consumed by the `sqlx::query!` macros during the build and never
read by the resulting binaries: at runtime the cache path comes from `--db-path`,
`GLAUCA_DB_PATH`, or the default. Running `cargo run` with `DATABASE_URL` set therefore still
opens the real cache — use `GLAUCA_DB_PATH` to redirect it.

When you add or change a `sqlx::query!`, refresh the offline cache with
`cargo sqlx prepare --workspace -- --all-targets`. The `-- --all-targets` is required: the bare
form runs `cargo check --workspace` without it, so it never sees queries inside `#[cfg(test)]`
modules and deletes their cached entries — which breaks CI, since CI builds with
`SQLX_OFFLINE=true cargo clippy --all-targets` and fails complaining that a query is missing from
the offline cache.

A new migration must also be applied to the development database before code referencing its
columns will compile: `cargo sqlx migrate run --source crates/glauca-core/migrations`.

## License

Released under the [MIT License](LICENSE).
