## 0.2.1 (2026-07-21)

### Features

- support OR groups in filter-stream matching ([#54](https://github.com/okkez/glauca/pull/54) by @okkez)
- edit filter streams as multiple OR boxes ([#54](https://github.com/okkez/glauca/pull/54) by @okkez)
- edit filter streams as multiple OR boxes ([#54](https://github.com/okkez/glauca/pull/54) by @okkez)
- edit filter streams as multiple OR boxes ([#54](https://github.com/okkez/glauca/pull/54) by @okkez)
- support mouse interactions ([#56](https://github.com/okkez/glauca/pull/56) by @okkez)

### Fixes

- preserve OR-group boundaries when expanding @me ([#54](https://github.com/okkez/glauca/pull/54) by @okkez)
- correct mouse double-click, scroll focus, and pane hit-testing ([#56](https://github.com/okkez/glauca/pull/56) by @okkez)
- harden mouse double-click against stray triggers ([#56](https://github.com/okkez/glauca/pull/56) by @okkez)
- coalesce background sync jobs per query ([#57](https://github.com/okkez/glauca/pull/57) by @okkez)

## 0.2.0 (2026-07-15)

### Breaking Changes

- Forced major bump via the bump:major label.

### Features

- add background cache maintenance to bound cache.db growth ([#49](https://github.com/okkez/glauca/pull/49) by @okkez)
- re-fetch cleared item bodies transparently on open ([#49](https://github.com/okkez/glauca/pull/49) by @okkez)
- cycle TUI panes with Tab and Shift+Tab ([#50](https://github.com/okkez/glauca/pull/50) by @okkez)
- exit filter input with Tab ([#52](https://github.com/okkez/glauca/pull/52) by @okkez)
- exit filter input with Enter ([#52](https://github.com/okkez/glauca/pull/52) by @okkez)
- add label-driven version override for the release PR ([#53](https://github.com/okkez/glauca/pull/53) by @okkez)

### Fixes

- harden cache maintenance per code review ([#49](https://github.com/okkez/glauca/pull/49) by @okkez)

## 0.1.9 (2026-07-12)

### Features

- add Tauri desktop frontend reusing glauca-core ([#36](https://github.com/okkez/glauca/pull/36) by @okkez)
- expand glauca-tauri toward glauca-gui feature parity ([#36](https://github.com/okkez/glauca/pull/36) by @okkez)
- run custom user-defined actions from the detail pane ([#36](https://github.com/okkez/glauca/pull/36) by @okkez)
- keyboard navigation matching the TUI/GUI keymap ([#36](https://github.com/okkez/glauca/pull/36) by @okkez)
- show the signed-in user in the status bar ([#36](https://github.com/okkez/glauca/pull/36) by @okkez)
- scroll the detail body with j/k when the detail pane is focused ([#34](https://github.com/okkez/glauca/pull/34) by @okkez)
- surface engine errors as notification toasts ([#34](https://github.com/okkez/glauca/pull/34) by @okkez)
- focus the detail pane and scroll its body with j/k ([#36](https://github.com/okkez/glauca/pull/36) by @okkez)
- support --version and --help flags ([#41](https://github.com/okkez/glauca/pull/41) by @okkez)
- restructure the layout around a GUI-style menu bar and sidebar ([#45](https://github.com/okkez/glauca/pull/45) by @okkez)
- inline GitHub Octicons and the GUI's state-icon mapping ([#45](https://github.com/okkez/glauca/pull/45) by @okkez)
- rebuild item rows on the GUI's three-line layout ([#45](https://github.com/okkez/glauca/pull/45) by @okkez)
- highlight inline-filter matches in item titles ([#45](https://github.com/okkez/glauca/pull/45) by @okkez)
- rebuild the detail pane on the GUI's pinned-header layout ([#45](https://github.com/okkez/glauca/pull/45) by @okkez)
- drag-to-resize panes with persisted widths ([#45](https://github.com/okkez/glauca/pull/45) by @okkez)
- support `-` prefix negation in local filter queries ([#46](https://github.com/okkez/glauca/pull/46) by @okkez)

### Fixes

- port front-end bridge to the Jasper unread model ([#36](https://github.com/okkez/glauca/pull/36) by @okkez)
- adopt the per-item Jasper unread model in the web UI ([#36](https://github.com/okkez/glauca/pull/36) by @okkez)
- surface IPC errors, set a CSP, harden the message loop ([#36](https://github.com/okkez/glauca/pull/36) by @okkez)
- address code-review findings before merge ([#34](https://github.com/okkez/glauca/pull/34) by @okkez)
- write settings atomically to avoid losing them on a torn write ([#35](https://github.com/okkez/glauca/pull/35) by @okkez)
- gate entry CRUD keys to the entries pane ([#36](https://github.com/okkez/glauca/pull/36) by @okkez)
- guard refreshVisible against a stale filter result ([#36](https://github.com/okkez/glauca/pull/36) by @okkez)
- address minor code-review findings (atomic save, save order, menu leak, modal Escape) ([#36](https://github.com/okkez/glauca/pull/36) by @okkez)
- create the log directory before pruning old files ([#42](https://github.com/okkez/glauca/pull/42) by @okkez)
- deliver engine messages push-based instead of polling ([#43](https://github.com/okkez/glauca/pull/43) by @okkez)
- repaint immediately on j/k moves in the item list ([#43](https://github.com/okkez/glauca/pull/43) by @okkez)
- drop pending background items instead of showing a 0-updated banner ([#45](https://github.com/okkez/glauca/pull/45) by @okkez)
- address code-review findings in the GUI-alignment work ([#45](https://github.com/okkez/glauca/pull/45) by @okkez)
- bump glauca-tauri in Cargo.lock via knope versioned_files ([#47](https://github.com/okkez/glauca/pull/47) by @okkez)

## 0.1.8 (2026-07-03)

### Features

- pass the target repo's local checkout to octorus via --working-dir ([#30](https://github.com/okkez/glauca/pull/30) by @okkez)
- add Custom actions submenu to the item context menu ([#31](https://github.com/okkez/glauca/pull/31) by @okkez)
- fuzzy-match plain-text filter tokens

## 0.1.7 (2026-07-01)

### Features

- show the GitHub logo before saved queries in the left pane ([#25](https://github.com/okkez/glauca/pull/25) by @okkez)
- open item in browser on Shift+click ([#27](https://github.com/okkez/glauca/pull/27) by @okkez)
- run custom user-defined actions on the selected item ([#28](https://github.com/okkez/glauca/pull/28) by @okkez)
- support cursor movement in text input fields ([#29](https://github.com/okkez/glauca/pull/29) by @okkez)

### Fixes

- space out icons in reviewers and review detail lines ([#25](https://github.com/okkez/glauca/pull/25) by @okkez)
- space out left-pane query icons and theme the stream marker ([#25](https://github.com/okkez/glauca/pull/25) by @okkez)

## 0.1.6 (2026-06-26)

### Features

- combine state and kind into one item-list icon ([#22](https://github.com/okkez/glauca/pull/22) by @okkez)

## 0.1.5 (2026-06-24)

### Features

- support more GitHub qualifiers in local filter streams ([#14](https://github.com/okkez/glauca/pull/14) by @okkez)
- mark parent query rows with a 🔍 badge ([#16](https://github.com/okkez/glauca/pull/16) by @okkez)
- show created in detail header and render times in local zone ([#17](https://github.com/okkez/glauca/pull/17) by @okkez)
- add opt-in Nerd Font icon set toggled with F

### Fixes

- match is:pr / is:issue in local filter streams ([#14](https://github.com/okkez/glauca/pull/14) by @okkez)

## 0.1.4 (2026-06-23)

### Features

- redefine unread as update-driven (Jasper-style) ([#11](https://github.com/okkez/glauca/pull/11) by @okkez)

## 0.1.3 (2026-06-22)

### Fixes

- fall back to `gh auth token` so the gh extension is authenticated ([#9](https://github.com/okkez/glauca/pull/9) by @okkez)

## 0.1.2 (2026-06-20)

### Features

- link each changelog entry to its PR

### Fixes

- install rustls ring CryptoProvider at startup ([#6](https://github.com/okkez/glauca/pull/6) by @okkez)

## 0.1.1 (2026-06-20)

### Fixes

- mirror zed's async-process patch to fix macOS GUI build
