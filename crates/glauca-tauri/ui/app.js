// glauca-tauri front-end.
//
// Uses the global Tauri API (`withGlobalTauri: true` in tauri.conf.json), so no
// npm/@tauri-apps/api and no build step — the file is served as-is. Two channels:
//   * invoke('<command>', {...})  → engine (commands.rs); args are camelCase and
//     Tauri maps them to the snake_case Rust params.
//   * listen('app-message', ...)  ← engine; payload is the adjacently-tagged
//     AppMessage: { type: "ItemsLoaded", data: {...} }.
//
// Feature-wise this mirrors the TUI/GUI: browse / filter / sync / read / act,
// entry CRUD + reorder, custom actions, and keyboard navigation.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const state = {
  entries: [],            // normalized left-pane entries
  rawEntries: [],         // original LeftPaneEntry JSON ({type,data}) for unread_counts
  currentUser: null,
  itemsByQuery: new Map(), // rootQueryId -> ItemEntry[]
  pending: new Map(),      // rootQueryId -> held-back background items (not yet applied)
  visibleItems: [],        // current entry's items after stream + inline filtering
  visibleTitleSegments: [], // per visibleItems row: pre-split {text, highlighted}
                          // title segments from filter_items (empty = no highlight)
  unread: new Map(),       // unreadKey(isFilterStream, entryId) -> count
  selectedEntry: -1,
  selectedItemKey: null,
  focus: "entries",        // keyboard focus: "entries" | "items" | "detail" (h/l to switch)
  filterText: "",
  comments: [],            // last-loaded comments (shown in the comments modal)
  commentsShowMinimized: false,
  commentsSortNewest: false,
  settings: { theme: "system", notifications_enabled: false, sync_interval_secs: 60, pane_sizes: null },
  filterSeq: 0,            // bumped per refreshVisible() call; guards against a stale
                          // filter_items result overwriting a newer entry's items
  syncingCount: 0,         // in-flight foreground syncs (SyncStarted − SyncDone/Error)
  bgSyncPending: 0,        // queued background-sync jobs (BgSyncQueued − BgSyncJobDone)
  bodyRefreshRequested: new Set(), // itemKey()s whose cleared body we've already
                          // asked to re-fetch this session (dedup for selectItem)
};

// ── helpers ──────────────────────────────────────────────────────────────────

const $ = (id) => document.getElementById(id);

function el(tag, opts = {}, children = []) {
  const node = document.createElement(tag);
  if (opts.class) node.className = opts.class;
  if (opts.text != null) node.textContent = opts.text;
  if (opts.onclick) node.addEventListener("click", opts.onclick);
  for (const c of children) if (c) node.appendChild(c);
  return node;
}

// Latest status message, rendered into the sidebar footer (the GUI keeps its
// status in the left pane's footer rather than a global status bar).
let statusMsg = "";
let statusIsError = false;

function setStatus(msg, isError = false) {
  statusMsg = msg;
  statusIsError = isError;
  renderFooter();
}

// Sidebar footer: sync activity ("syncing…", "N bg") plus the latest status
// message. Hidden entirely when there is nothing to show, like the GUI.
function renderFooter() {
  const footer = $("sidebar-footer");
  const bits = [];
  if (state.syncingCount > 0) bits.push("syncing…");
  if (state.bgSyncPending > 0) bits.push(`${state.bgSyncPending} bg`);
  const rows = [];
  if (bits.length) rows.push(el("div", { text: bits.join("  ") }));
  if (statusMsg) rows.push(el("div", { class: "status-line", text: statusMsg }));
  footer.classList.toggle("error", statusIsError);
  footer.replaceChildren(...rows);
  footer.hidden = rows.length === 0;
}

// invoke() for fire-and-forget commands: report failures on the sidebar
// footer instead of losing them as unhandled rejections. Use plain invoke()
// (with await / .catch) when the result matters.
function call(cmd, args) {
  return invoke(cmd, args).catch((e) => setStatus(`${cmd}: ${e}`, true));
}

function itemKey(it) {
  return `${it.repo_owner}/${it.repo_name}#${it.number}`;
}

// ── octicons ─────────────────────────────────────────────────────────────────
//
// GitHub Octicons (MIT licensed, https://github.com/primer/octicons), the same
// set the GUI vendors under crates/glauca-gui/assets/octicons. Inlined as path
// data because the CSP has no connect-src 'self' (fetch() of an .svg would be
// blocked), and `fill: currentColor` lets CSS color them per state/theme.

const OCTICONS = {
  "check-circle-fill": [
    { d: "M8 16A8 8 0 1 0 8 0a8 8 0 0 0 0 16Zm3.78-9.72a.751.751 0 0 0-.018-1.042.751.751 0 0 0-1.042-.018L6.75 9.19 5.28 7.72a.751.751 0 0 0-1.042.018.751.751 0 0 0-.018 1.042l2 2a.75.75 0 0 0 1.06 0Z", evenodd: true },
  ],
  clock: [
    { d: "M8 0a8 8 0 1 1 0 16A8 8 0 0 1 8 0ZM1.5 8a6.5 6.5 0 1 0 13 0 6.5 6.5 0 0 0-13 0Zm7-3.25v2.992l2.028.812a.75.75 0 0 1-.557 1.392l-2.5-1A.751.751 0 0 1 7 8.25v-3.5a.75.75 0 0 1 1.5 0Z" },
  ],
  comment: [
    { d: "M1 2.75C1 1.784 1.784 1 2.75 1h10.5c.966 0 1.75.784 1.75 1.75v7.5A1.75 1.75 0 0 1 13.25 12H9.06l-2.573 2.573A1.458 1.458 0 0 1 4 13.543V12H2.75A1.75 1.75 0 0 1 1 10.25Zm1.75-.25a.25.25 0 0 0-.25.25v7.5c0 .138.112.25.25.25h2a.75.75 0 0 1 .75.75v2.19l2.72-2.72a.749.749 0 0 1 .53-.22h4.5a.25.25 0 0 0 .25-.25v-7.5a.25.25 0 0 0-.25-.25Z" },
  ],
  "git-merge": [
    { d: "M5.45 5.154A4.25 4.25 0 0 0 9.25 7.5h1.378a2.251 2.251 0 1 1 0 1.5H9.25A5.734 5.734 0 0 1 5 7.123v3.505a2.25 2.25 0 1 1-1.5 0V5.372a2.25 2.25 0 1 1 1.95-.218ZM4.25 13.5a.75.75 0 1 0 0-1.5.75.75 0 0 0 0 1.5Zm8.5-4.5a.75.75 0 1 0 0-1.5.75.75 0 0 0 0 1.5ZM5 3.25a.75.75 0 1 0 0 .005V3.25Z" },
  ],
  "git-pull-request": [
    { d: "M1.5 3.25a2.25 2.25 0 1 1 3 2.122v5.256a2.251 2.251 0 1 1-1.5 0V5.372A2.25 2.25 0 0 1 1.5 3.25Zm5.677-.177L9.573.677A.25.25 0 0 1 10 .854V2.5h1A2.5 2.5 0 0 1 13.5 5v5.628a2.251 2.251 0 1 1-1.5 0V5a1 1 0 0 0-1-1h-1v1.646a.25.25 0 0 1-.427.177L7.177 3.427a.25.25 0 0 1 0-.354ZM3.75 2.5a.75.75 0 1 0 0 1.5.75.75 0 0 0 0-1.5Zm0 9.5a.75.75 0 1 0 0 1.5.75.75 0 0 0 0-1.5Zm8.25.75a.75.75 0 1 0 1.5 0 .75.75 0 0 0-1.5 0Z" },
  ],
  "git-pull-request-closed": [
    { d: "M3.25 1A2.25 2.25 0 0 1 4 5.372v5.256a2.251 2.251 0 1 1-1.5 0V5.372A2.251 2.251 0 0 1 3.25 1Zm9.5 5.5a.75.75 0 0 1 .75.75v3.378a2.251 2.251 0 1 1-1.5 0V7.25a.75.75 0 0 1 .75-.75Zm-2.03-5.273a.75.75 0 0 1 1.06 0l.97.97.97-.97a.748.748 0 0 1 1.265.332.75.75 0 0 1-.205.729l-.97.97.97.97a.751.751 0 0 1-.018 1.042.751.751 0 0 1-1.042.018l-.97-.97-.97.97a.749.749 0 0 1-1.275-.326.749.749 0 0 1 .215-.734l.97-.97-.97-.97a.75.75 0 0 1 0-1.06ZM2.5 3.25a.75.75 0 1 0 1.5 0 .75.75 0 0 0-1.5 0ZM3.25 12a.75.75 0 1 0 0 1.5.75.75 0 0 0 0-1.5Zm9.5 0a.75.75 0 1 0 0 1.5.75.75 0 0 0 0-1.5Z" },
  ],
  "git-pull-request-draft": [
    { d: "M3.25 1A2.25 2.25 0 0 1 4 5.372v5.256a2.251 2.251 0 1 1-1.5 0V5.372A2.251 2.251 0 0 1 3.25 1Zm9.5 14a2.25 2.25 0 1 1 0-4.5 2.25 2.25 0 0 1 0 4.5ZM2.5 3.25a.75.75 0 1 0 1.5 0 .75.75 0 0 0-1.5 0ZM3.25 12a.75.75 0 1 0 0 1.5.75.75 0 0 0 0-1.5Zm9.5 0a.75.75 0 1 0 0 1.5.75.75 0 0 0 0-1.5ZM14 7.5a1.25 1.25 0 1 1-2.5 0 1.25 1.25 0 0 1 2.5 0Zm0-4.25a1.25 1.25 0 1 1-2.5 0 1.25 1.25 0 0 1 2.5 0Z" },
  ],
  "issue-closed": [
    { d: "M11.28 6.78a.75.75 0 0 0-1.06-1.06L7.25 8.69 5.78 7.22a.75.75 0 0 0-1.06 1.06l2 2a.75.75 0 0 0 1.06 0l3.5-3.5Z" },
    { d: "M16 8A8 8 0 1 1 0 8a8 8 0 0 1 16 0Zm-1.5 0a6.5 6.5 0 1 0-13 0 6.5 6.5 0 0 0 13 0Z" },
  ],
  "issue-opened": [
    { d: "M8 9.5a1.5 1.5 0 1 0 0-3 1.5 1.5 0 0 0 0 3Z" },
    { d: "M8 0a8 8 0 1 1 0 16A8 8 0 0 1 8 0ZM1.5 8a6.5 6.5 0 1 0 13 0 6.5 6.5 0 0 0-13 0Z" },
  ],
  lock: [
    { d: "M4 4a4 4 0 0 1 8 0v2h.25c.966 0 1.75.784 1.75 1.75v5.5A1.75 1.75 0 0 1 12.25 15h-8.5A1.75 1.75 0 0 1 2 13.25v-5.5C2 6.784 2.784 6 3.75 6H4Zm8.25 3.5h-8.5a.25.25 0 0 0-.25.25v5.5c0 .138.112.25.25.25h8.5a.25.25 0 0 0 .25-.25v-5.5a.25.25 0 0 0-.25-.25ZM10.5 6V4a2.5 2.5 0 1 0-5 0v2Z" },
  ],
  people: [
    { d: "M2 5.5a3.5 3.5 0 1 1 5.898 2.549 5.508 5.508 0 0 1 3.034 4.084.75.75 0 1 1-1.482.235 4 4 0 0 0-7.9 0 .75.75 0 0 1-1.482-.236A5.507 5.507 0 0 1 3.102 8.05 3.493 3.493 0 0 1 2 5.5ZM11 4a3.001 3.001 0 0 1 2.22 5.018 5.01 5.01 0 0 1 2.56 3.012.749.749 0 0 1-.885.954.752.752 0 0 1-.549-.514 3.507 3.507 0 0 0-2.522-2.372.75.75 0 0 1-.574-.73v-.352a.75.75 0 0 1 .416-.672A1.5 1.5 0 0 0 11 5.5.75.75 0 0 1 11 4Zm-5.5-.5a2 2 0 1 0-.001 3.999A2 2 0 0 0 5.5 3.5Z" },
  ],
  "x-circle-fill": [
    { d: "M2.343 13.657A8 8 0 1 1 13.658 2.342 8 8 0 0 1 2.343 13.657ZM6.03 4.97a.751.751 0 0 0-1.042.018.751.751 0 0 0-.018 1.042L6.94 8 4.97 9.97a.749.749 0 0 0 .326 1.275.749.749 0 0 0 .734-.215L8 9.06l1.97 1.97a.749.749 0 0 0 1.275-.326.749.749 0 0 0-.215-.734L9.06 8l1.97-1.97a.749.749 0 0 0-.326-1.275.749.749 0 0 0-.734.215L8 6.94Z", evenodd: true },
  ],
};

// An inline <svg> for the named octicon, colored via `currentColor` (so a color
// class like .state-open on the element tints it, matching how gpui paints the
// same SVGs as masks).
function octicon(name, cls = "", size = 16) {
  const NS = "http://www.w3.org/2000/svg";
  const svg = document.createElementNS(NS, "svg");
  svg.setAttribute("viewBox", "0 0 16 16");
  svg.setAttribute("width", String(size));
  svg.setAttribute("height", String(size));
  svg.setAttribute("fill", "currentColor");
  svg.setAttribute("aria-hidden", "true");
  svg.setAttribute("class", cls ? `octicon ${cls}` : "octicon");
  for (const p of OCTICONS[name] || []) {
    const path = document.createElementNS(NS, "path");
    path.setAttribute("d", p.d);
    if (p.evenodd) path.setAttribute("fill-rule", "evenodd");
    svg.appendChild(path);
  }
  return svg;
}

// Jasper-style unread: the item changed since it was last read. Mirrors
// glauca_core::logic::is_item_unread; both timestamps are RFC3339 UTC, so plain
// string comparison orders correctly (same assumption core makes).
function isUnread(it) {
  return it.last_read_updated_at == null || it.updated_at > it.last_read_updated_at;
}

// Show/hide the change banner based on held-back background items for the
// currently-selected query. Clicking it applies the pending items. Both the diff
// and its wording come from core (via count_item_changes), so this matches the
// TUI/GUI exactly — including counting removals, so a sync that only pruned items
// no longer matching the query isn't mistaken for "nothing changed".
async function updateBanner() {
  const banner = $("banner");
  const e = state.entries[state.selectedEntry];
  const fresh = e && state.pending.get(e.rootQueryId);
  if (!fresh) {
    banner.hidden = true;
    return;
  }
  const current = state.itemsByQuery.get(e.rootQueryId) || [];
  let changes;
  try {
    changes = await invoke("count_item_changes", { current, fresh });
  } catch {
    // The banner is display-only, so a vague label beats no banner at all: losing
    // the count must not lose the affordance, or a sync that pruned everything
    // would silently keep the stale rows on screen.
    changes = { total: 1, label: "updated in background" };
  }
  // The selection may have moved while the count was in flight; a stale
  // resolution must not repaint the banner for a query no longer shown
  // (previewEntry already re-ran updateBanner for the new one).
  const cur = state.entries[state.selectedEntry];
  if (!cur || cur.rootQueryId !== e.rootQueryId) return;
  if (changes.total === 0) {
    // Nothing user-visible changed; drop the held-back items instead of
    // showing an empty banner (the GUI clears pending the same way).
    state.pending.delete(e.rootQueryId);
    banner.hidden = true;
    return;
  }
  banner.textContent = `${changes.label} — click to refresh`;
  banner.hidden = false;
  banner.onclick = () => applyPending(e.rootQueryId);
}

// Apply held-back background items for a query (the user opted in via the banner).
function applyPending(queryId) {
  const items = state.pending.get(queryId);
  if (!items) return;
  state.pending.delete(queryId);
  state.itemsByQuery.set(queryId, items);
  const e = state.entries[state.selectedEntry];
  if (e && e.rootQueryId === queryId) refreshVisible();
  refreshUnread(queryId);
  updateBanner();
}

// Copy text to the clipboard, falling back to a hidden textarea for webviews
// without async clipboard access.
async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    setStatus("URL copied");
    return;
  } catch {
    /* fall through */
  }
  const ta = el("textarea");
  ta.value = text;
  ta.style.position = "fixed";
  ta.style.opacity = "0";
  document.body.appendChild(ta);
  ta.select();
  try {
    document.execCommand("copy");
    setStatus("URL copied");
  } catch {
    setStatus("copy failed", true);
  }
  ta.remove();
}

// Flatten an init `LeftPaneEntry` ({type, data}) into a uniform shape.
function normalize(e) {
  const d = e.data;
  if (e.type === "Query") {
    return {
      id: d.id,
      isFilterStream: false,
      label: d.label,
      kind: d.kind,
      queryStr: d.query_str,
      rootQueryId: d.id,
      streamFilter: null,
    };
  }
  return {
    id: d.id,
    isFilterStream: true,
    label: d.name,
    kind: d.kind,
    queryStr: null,
    rootQueryId: d.parent_id,
    streamFilter: d.filter,
  };
}

function unreadKey(isFilterStream, entryId) {
  return `${isFilterStream ? 1 : 0}:${entryId}`;
}

// Recompute unread badges for every entry under `rootQueryId` by delegating to
// the engine's unread_counts command, which reuses glauca-core's
// compute_unread_counts — correct filter-stream scoping and the same
// Jasper-style unread definition (updated_at > last_read_updated_at) the TUI/GUI
// use. Driven by the front-end's in-memory items so it reflects reads
// immediately (no DB round-trip race after mark_item_read).
async function refreshUnread(rootQueryId) {
  const items = state.itemsByQuery.get(rootQueryId) || [];
  try {
    const counts = await invoke("unread_counts", {
      entries: state.rawEntries,
      queryId: rootQueryId,
      items,
    });
    for (const c of counts) {
      state.unread.set(unreadKey(c.is_filter_stream, c.entry_id), c.count);
    }
    renderSidebar();
  } catch (e) {
    setStatus(`unread: ${e}`, true);
  }
}

// Multi-field in-page form modal. Replaces window.prompt() (unreliable across
// Tauri/wry webviews, notably macOS WKWebView). `fields` is an array of
// {key, label, value?, required?}. Resolves to an object keyed by field.key, or
// null if cancelled.
function formModal(title, fields) {
  return new Promise((resolve) => {
    const overlay = el("div", { class: "modal-overlay" });
    const finish = (val) => {
      overlay.remove();
      resolve(val);
    };
    const inputs = fields.map((f) => {
      const input = el("input", { class: "modal-input" });
      input.type = "text";
      input.value = f.value || "";
      input.placeholder = f.label;
      return { f, input };
    });
    const submit = () => {
      const out = {};
      for (const { f, input } of inputs) {
        const v = input.value.trim();
        if (f.required && !v) {
          input.focus();
          return;
        }
        out[f.key] = v;
      }
      finish(out);
    };
    const rows = inputs.flatMap(({ f, input }) => [el("div", { class: "modal-label", text: f.label }), input]);
    const ok = el("button", { text: "OK", onclick: submit });
    const cancel = el("button", { text: "Cancel", onclick: () => finish(null) });
    for (const { input } of inputs) {
      input.addEventListener("keydown", (ev) => {
        if (ev.key === "Enter") submit();
        else if (ev.key === "Escape") finish(null);
      });
    }
    overlay.appendChild(
      el("div", { class: "modal-box" }, [
        el("div", { class: "modal-title", text: title }),
        ...rows,
        el("div", { class: "modal-actions" }, [cancel, ok]),
      ])
    );
    document.body.appendChild(overlay);
    if (inputs[0]) inputs[0].input.focus();
  });
}

// Single-field convenience wrapper over formModal (used for comment/merge input).
async function promptModal(label, def = "") {
  const out = await formModal(label, [{ key: "value", label, value: def }]);
  return out ? out.value : null;
}

// Group separator inside a stored filter-stream filter. Mirrors the Rust
// `glauca_core::filter::FILTER_GROUP_SEP` — this is a cross-language contract
// (the backend splits/joins on the same char), so keep the two in sync.
const FILTER_GROUP_SEP = "\n";

// Split a stored filter-stream filter into its OR-group boxes (one group per
// separator; see glauca_core::filter::StreamFilter). Always yields at least one box.
function splitFilterGroups(s) {
  return (s || "").split(FILTER_GROUP_SEP);
}

// Filter-stream create/edit modal: a name field plus one or more OR-group boxes
// (each box is one AND-group; the boxes are ORed, mirroring the TUI). Add/remove
// boxes with the buttons. Resolves { name, filter } with `filter` as the
// newline-joined non-blank groups, or null on cancel.
function filterStreamModal(title, initName, initFilters) {
  return new Promise((resolve) => {
    const overlay = el("div", { class: "modal-overlay" });
    const finish = (val) => {
      overlay.remove();
      resolve(val);
    };

    const onKey = (ev) => {
      if (ev.key === "Enter") submit();
      else if (ev.key === "Escape") finish(null);
    };

    const nameInput = el("input", { class: "modal-input" });
    nameInput.type = "text";
    nameInput.value = initName || "";
    nameInput.placeholder = "Name";
    nameInput.addEventListener("keydown", onKey);

    // Live <input> elements, reused across rebuilds so their values survive.
    const makeBox = (v) => {
      const inp = el("input", { class: "modal-input" });
      inp.type = "text";
      inp.value = v || "";
      inp.placeholder = "e.g. is:pr is:draft assignee:name";
      inp.addEventListener("keydown", onKey);
      return inp;
    };
    let boxes = (initFilters && initFilters.length ? initFilters : [""]).map(makeBox);

    const filtersWrap = el("div", { class: "filter-boxes" });
    const rebuild = () => {
      filtersWrap.textContent = "";
      boxes.forEach((box, i) => {
        if (i > 0) filtersWrap.appendChild(el("div", { class: "filter-or", text: "OR" }));
        const remove = el("button", {
          class: "filter-remove",
          text: "✕",
          // Disabled when it's the only box, so the click can't drop below one.
          onclick: () => {
            boxes.splice(i, 1);
            rebuild();
            boxes[Math.min(i, boxes.length - 1)].focus();
          },
        });
        remove.type = "button";
        remove.disabled = boxes.length === 1;
        filtersWrap.appendChild(el("div", { class: "filter-row" }, [box, remove]));
      });
    };

    const addBox = () => {
      const b = makeBox("");
      boxes.push(b);
      rebuild();
      b.focus();
    };

    const submit = () => {
      const name = nameInput.value.trim();
      const groups = boxes.map((b) => b.value.trim()).filter((v) => v);
      if (!name) {
        nameInput.focus();
        return;
      }
      if (!groups.length) {
        boxes[0].focus();
        return;
      }
      finish({ name, filter: groups.join(FILTER_GROUP_SEP) });
    };

    const addBtn = el("button", { class: "filter-add", text: "+ Add OR box", onclick: addBox });
    addBtn.type = "button";
    const ok = el("button", { text: "OK", onclick: submit });
    const cancel = el("button", { text: "Cancel", onclick: () => finish(null) });

    rebuild();
    overlay.appendChild(
      el("div", { class: "modal-box" }, [
        el("div", { class: "modal-title", text: title }),
        el("div", { class: "modal-label", text: "Name" }),
        nameInput,
        el("div", { class: "modal-label", text: "Filters (item matches ANY box)" }),
        filtersWrap,
        addBtn,
        el("div", { class: "modal-actions" }, [cancel, ok]),
      ])
    );
    document.body.appendChild(overlay);
    nameInput.focus();
  });
}

// Yes/No confirmation modal. Resolves true if confirmed, false otherwise.
function confirmModal(message) {
  return new Promise((resolve) => {
    const overlay = el("div", { class: "modal-overlay" });
    const finish = (val) => {
      overlay.remove();
      resolve(val);
    };
    const ok = el("button", { text: "Delete", class: "danger", onclick: () => finish(true) });
    const cancel = el("button", { text: "Cancel", onclick: () => finish(false) });
    overlay.appendChild(
      el("div", { class: "modal-box" }, [
        el("div", { class: "modal-label", text: message }),
        el("div", { class: "modal-actions" }, [cancel, ok]),
      ])
    );
    document.body.appendChild(overlay);
  });
}

// A dismissable read-only modal (About / Help): overlay + title + body rows +
// a Close button; any of `closeKeys` also dismisses it. Owns the pairing of
// its capture-phase keydown listener with every close path, so callers can't
// leak the listener.
function infoModal(title, bodyEls, { closeKeys = ["Escape", "q"], boxClass = "" } = {}) {
  document.querySelectorAll(".modal-overlay").forEach((m) => m.remove());
  const overlay = el("div", { class: "modal-overlay" });
  const close = () => {
    overlay.remove();
    document.removeEventListener("keydown", onKey, true);
  };
  const onKey = (ev) => {
    if (!closeKeys.includes(ev.key)) return;
    ev.preventDefault();
    close();
  };
  document.addEventListener("keydown", onKey, true);
  overlay.appendChild(
    el("div", { class: boxClass ? `modal-box ${boxClass}` : "modal-box" }, [
      el("div", { class: "modal-title", text: title }),
      ...bodyEls,
      el("div", { class: "modal-actions" }, [el("button", { text: "Close", onclick: close })]),
    ])
  );
  document.body.appendChild(overlay);
}

// Lightweight context menu at (x, y). `items` is [{label, onClick}] (a null entry
// renders a separator). Closes on selection or any outside click/Escape.
// The document-level close listener for the open context menu, tracked so it can
// be unregistered when the menu goes away (via dismiss, item click, or a new menu
// opening). Without this the listeners outlived their detached menu node.
let ctxMenuClose = null;

// Menu-bar button id whose dropdown is currently open (null for ordinary
// context menus). First-class ownership lets wireMenuButton decide toggle-close
// vs. open from the *current* state, with no dismissal-history heuristics.
let menuBarOwner = null;

// Remove any open context menu AND its document-level close listeners.
function dismissContextMenu() {
  document.querySelectorAll(".ctx-menu").forEach((m) => m.remove());
  menuBarOwner = null;
  if (ctxMenuClose) {
    document.removeEventListener("mousedown", ctxMenuClose, true);
    document.removeEventListener("keydown", ctxMenuClose, true);
    ctxMenuClose = null;
  }
}

// `ownerEl` (optional) is the element that opened the menu: mousedowns inside
// it don't auto-dismiss, so its own click handler stays the single decision
// point (used by the menu-bar buttons for one-click toggling).
function showContextMenu(x, y, items, ownerEl = null) {
  dismissContextMenu(); // clear any prior menu and its listeners first
  const menu = el("div", { class: "ctx-menu" });
  menu.style.left = `${x}px`;
  menu.style.top = `${y}px`;
  for (const it of items) {
    if (!it) {
      menu.appendChild(el("div", { class: "ctx-sep" }));
      continue;
    }
    menu.appendChild(
      el("div", {
        class: "ctx-item",
        text: it.label,
        onclick: () => {
          dismissContextMenu();
          it.onClick();
        },
      })
    );
  }
  const close = (ev) => {
    if (ev.type === "keydown" && ev.key !== "Escape") return;
    if (ev.type === "mousedown") {
      // Clicks inside the menu are the items' own onclick business; closing
      // (and detaching the node) on the capture-phase mousedown would race the
      // click. Same for the owner element — its click handler decides open
      // vs. toggle.
      const insideMenu = ev.target.closest(".ctx-menu");
      const insideOwner = ownerEl && ownerEl.contains(ev.target);
      if (insideMenu || insideOwner) return;
    }
    dismissContextMenu();
  };
  ctxMenuClose = close;
  document.addEventListener("mousedown", close, true);
  document.addEventListener("keydown", close, true);
  document.body.appendChild(menu);
}

// ── menu bar ──────────────────────────────────────────────────────────────---
//
// HTML buttons + the shared context menu, mirroring the GUI's menu bar
// (Glauca / View / Help dropdowns).

// "✓ " prefix on the active option; NBSPs keep inactive labels aligned.
function checkmark(active) {
  return active ? "✓ " : "   ";
}

function glaucaMenuItems() {
  const e = state.entries[state.selectedEntry];
  return [
    { label: "Sync now", onClick: () => syncEntry(e) },
    { label: "Full resync", onClick: () => fullResyncEntry(e) },
    null,
    { label: "Settings…", onClick: openSettingsModal },
    null,
    { label: "Quit", onClick: () => call("quit") },
  ];
}

function viewMenuItems() {
  const s = state.settings;
  const themeItem = (t, label) => ({
    label: `${checkmark(s.theme === t)}Theme: ${label}`,
    onClick: () => setTheme(t),
  });
  return [
    themeItem("system", "System"),
    themeItem("light", "Light"),
    themeItem("dark", "Dark"),
    null,
    {
      label: `${checkmark(s.notifications_enabled)}Desktop notifications`,
      onClick: () => persistSettings({ ...s, notifications_enabled: !s.notifications_enabled }),
    },
  ];
}

function helpMenuItems() {
  return [
    { label: "About Glauca", onClick: openAboutModal },
    { label: "Keyboard shortcuts", onClick: openHelpModal },
  ];
}

function openAboutModal() {
  const version = el("div", { class: "modal-label", text: "" });
  // Version comes from tauri.conf.json via the core app API (allowed by the
  // core:default capability).
  window.__TAURI__.app
    .getVersion()
    .then((v) => {
      version.textContent = `Version ${v}`;
    })
    .catch(() => {
      version.textContent = "";
    });
  infoModal("Glauca", [version]);
}

// Wire a menu-bar button: click opens the dropdown under the button; clicking
// it again while its own menu is open closes it (mousedowns on the owner are
// exempt from the auto-dismiss, so this handler sees the still-open state).
// A click on a different menu button switches menus in one click.
function wireMenuButton(id, buildItems) {
  const btn = $(id);
  btn.addEventListener("click", () => {
    if (menuBarOwner === id) {
      dismissContextMenu();
      return;
    }
    const r = btn.getBoundingClientRect();
    showContextMenu(Math.round(r.left), Math.round(r.bottom + 2), buildItems(), btn);
    menuBarOwner = id; // must come after showContextMenu — its dismissContextMenu() resets the owner
  });
}

// ── pane resize ─────────────────────────────────────────────────────────────-
//
// Drag the dividers to resize the fixed side panes, mirroring the GUI's
// resizable panes (same clamps: sidebar 250–560, detail ≥ 300; the flexible
// center keeps ≥ 250 by capping the dragged pane against the window width).
// Widths persist to settings.pane_sizes on drag end.

const SIDEBAR_MIN = 250;
const SIDEBAR_MAX = 560;
const DETAIL_MIN = 300;
const CENTER_MIN = 250;

// Restore persisted widths, re-clamped: the file may have been written on a
// larger window (or edited by hand), and unclamped values would overflow the
// flexible center below its minimum.
function applyPaneSizes(sizes) {
  if (!sizes) return;
  let [sidebar, detail] = sizes;
  if (sidebar > 0) {
    sidebar = Math.max(SIDEBAR_MIN, Math.min(SIDEBAR_MAX, sidebar));
    $("sidebar").style.width = `${sidebar}px`;
  }
  if (detail > 0) {
    const max = Math.max(DETAIL_MIN, window.innerWidth - paneWidth("sidebar") - CENTER_MIN);
    detail = Math.max(DETAIL_MIN, Math.min(max, detail));
    $("detail").style.width = `${detail}px`;
  }
}

function paneWidth(id) {
  return Math.round($(id).getBoundingClientRect().width);
}

function setupPaneResize() {
  const wire = (dividerId, resize) => {
    $(dividerId).addEventListener("mousedown", (ev) => {
      ev.preventDefault();
      const startX = ev.clientX;
      const startSidebar = paneWidth("sidebar");
      const startDetail = paneWidth("detail");
      const move = (mv) => resize(startSidebar, startDetail, mv.clientX - startX);
      const up = () => {
        document.removeEventListener("mousemove", move);
        document.removeEventListener("mouseup", up);
        persistSettings({ ...state.settings, pane_sizes: [paneWidth("sidebar"), paneWidth("detail")] });
      };
      document.addEventListener("mousemove", move);
      document.addEventListener("mouseup", up);
    });
  };
  wire("divider-left", (sidebar, detail, dx) => {
    const max = Math.min(SIDEBAR_MAX, window.innerWidth - detail - CENTER_MIN);
    const w = Math.max(SIDEBAR_MIN, Math.min(max, sidebar + dx));
    $("sidebar").style.width = `${w}px`;
  });
  wire("divider-right", (sidebar, detail, dx) => {
    const max = window.innerWidth - sidebar - CENTER_MIN;
    const w = Math.max(DETAIL_MIN, Math.min(max, detail - dx));
    $("detail").style.width = `${w}px`;
  });
}

// ── rendering ──────────────────────────────────────────────────────────────--

function renderSidebar() {
  const list = $("entries");
  list.replaceChildren();
  state.entries.forEach((e, idx) => {
    const cls = ["", e.isFilterStream ? "stream" : "", idx === state.selectedEntry ? "selected" : ""]
      .filter(Boolean)
      .join(" ");
    const children = [el("span", { class: "label", text: e.label })];
    // Unread badge for both root queries and filter streams, from core-computed
    // counts (see refreshUnread).
    const n = state.unread.get(unreadKey(e.isFilterStream, e.id)) || 0;
    if (n > 0) children.push(el("span", { class: "badge", text: String(n) }));
    const li = el("li", { class: cls, onclick: () => selectEntry(idx) }, children);
    li.addEventListener("contextmenu", (ev) => entryMenu(ev, idx));
    list.appendChild(li);
  });
}

// Recompute the visible item set for the current entry by delegating filtering to
// the engine's filter_items command, which reuses glauca-core's FilterQuery — the
// filter-stream filter ANDed with the inline search box, matching the TUI/GUI. The
// command returns matching indices, so visibleItems keeps the same object refs as
// itemsByQuery (last_read_updated_at advanced locally on read stays consistent).
async function refreshVisible() {
  const e = state.entries[state.selectedEntry];
  const all = e ? state.itemsByQuery.get(e.rootQueryId) || [] : [];
  if (!e) {
    state.visibleItems = [];
    renderItemList();
    return;
  }
  // Fast j/k launches overlapping filter_items calls; tag this one so a slow
  // earlier resolution can't overwrite a newer entry's items (which would show the
  // wrong list). Bail if a later call has already superseded us.
  const seq = ++state.filterSeq;
  try {
    const matches = await invoke("filter_items", {
      items: all,
      streamFilter: e.streamFilter,
      inlineFilter: state.filterText,
    });
    if (seq !== state.filterSeq) return;
    state.visibleItems = matches.map((m) => all[m.index]);
    state.visibleTitleSegments = matches.map((m) => m.title_segments);
  } catch (err) {
    if (seq !== state.filterSeq) return;
    setStatus(`filter: ${err}`, true);
    state.visibleItems = all;
    state.visibleTitleSegments = [];
  }
  renderItemList();
  // Also refresh the open detail pane from the reloaded items. renderDetail is
  // imperative and otherwise keeps showing the object captured at selectItem
  // time, so without this an updated body (e.g. a transparently re-fetched
  // maintenance-cleared body) or an applied background update would not appear
  // until reselect. No-op when nothing is selected or the selected item fell
  // out of the current view.
  if (state.selectedItemKey) {
    const selectedItem = state.visibleItems.find((x) => itemKey(x) === state.selectedItemKey);
    if (selectedItem) renderDetail(selectedItem);
  }
}

// Octicon name + color class encoding an item's state: the shape says
// issue-vs-PR, the color says open/merged/closed/draft. Mirrors the GUI's
// item_state_icon_info.
function itemStateIcon(it) {
  if (it.kind === "pull_request") {
    if (it.is_draft) return { name: "git-pull-request-draft", cls: "state-draft" };
    if (it.state === "merged") return { name: "git-merge", cls: "state-merged" };
    if (it.state === "closed") return { name: "git-pull-request-closed", cls: "state-closed" };
    return { name: "git-pull-request", cls: "state-open" };
  }
  return it.state === "closed"
    ? { name: "issue-closed", cls: "state-closed" }
    : { name: "issue-opened", cls: "state-open" };
}

// GitHub-style label for the detail-header state pill (mirrors state_label).
function stateLabel(it) {
  if (it.kind === "pull_request" && it.is_draft) return "Draft";
  if (it.state === "merged") return "Merged";
  if (it.state === "closed") return "Closed";
  return "Open";
}

// CSS modifier for a GitHub review state (mirrors logic::ReviewState::from_state).
function reviewStateClass(reviewState) {
  switch (reviewState) {
    case "APPROVED":
      return "rv-approved";
    case "CHANGES_REQUESTED":
      return "rv-changes";
    case "PENDING":
      return "rv-pending";
    case "DISMISSED":
      return "rv-dismissed";
    default:
      return "rv-commented";
  }
}

// Reviewers to show: everyone who submitted a review (with their state) plus
// requested reviewers who haven't reviewed yet (as PENDING). Mirrors
// glauca_core::logic::reviewer_overlays over data already present on the item.
function reviewerOverlays(it) {
  const out = (it.reviews || []).map(([user, state]) => ({ user, state }));
  for (const u of it.requested_reviewers || []) {
    if (!out.some((o) => o.user.login === u.login)) out.push({ user: u, state: "PENDING" });
  }
  return out;
}

// Max avatars per group (assignees / reviewers) before a "+N" overflow,
// matching the GUI's AVATAR_LIMIT.
const AVATAR_LIMIT = 5;

// GitHub serves a 460px avatar by default; downscaling that to a 24px <img>
// aliases badly. Ask GitHub to resize server-side to 2× the displayed size
// (HiDPI), mirroring the GUI's sized_avatar_url.
function sizedAvatarUrl(url, displayPx) {
  const sep = url.includes("?") ? "&" : "?";
  return `${url}${sep}s=${displayPx * 2}`;
}

// An avatar <img> (or an initial fallback when no avatar_url is known).
function avatarEl(user, cls = "avatar", displayPx = 24) {
  if (user && user.avatar_url) {
    const img = el("img", { class: cls });
    img.src = sizedAvatarUrl(user.avatar_url, displayPx);
    img.alt = user.login || "";
    img.title = user.login || "";
    return img;
  }
  const initial = (user && user.login ? user.login[0] : "?").toUpperCase();
  const span = el("span", { class: `${cls} avatar-fallback`, text: initial });
  if (user) span.title = user.login;
  return span;
}

// Octicon per review state for the badge overlay (the GUI's
// review_state_icon); COMMENTED / DISMISSED and anything unknown fall back to
// the comment icon.
const REVIEW_BADGE_ICON = {
  APPROVED: "check-circle-fill",
  CHANGES_REQUESTED: "x-circle-fill",
  PENDING: "clock",
};

// A reviewer avatar with a review-state octicon badge overlaid bottom-right
// (the GUI's reviewer_avatar): approved=green check, changes=red x,
// pending=yellow clock, commented/dismissed=grey comment.
function reviewerAvatar(user, reviewState) {
  const icon = REVIEW_BADGE_ICON[reviewState] || "comment";
  return el("span", { class: "rv-wrap" }, [
    avatarEl(user),
    octicon(icon, `rv-badge ${reviewStateClass(reviewState)}`, 14),
  ]);
}

// The item title with the inline-filter matches highlighted. `segments` come
// pre-split from filter_items ({text, highlighted} pieces — the Rust side owns
// the byte-offset arithmetic); empty/missing means no highlight.
function renderHighlightedTitle(title, segments) {
  const span = el("span", { class: "it-title" });
  if (!segments || !segments.length) {
    span.textContent = title;
    return span;
  }
  for (const s of segments) {
    if (s.highlighted) span.appendChild(el("span", { class: "hl", text: s.text }));
    else span.appendChild(document.createTextNode(s.text));
  }
  return span;
}

// Local "YYYY-MM-DD HH:MM:SS ±HH:MM", a port of core's format_local_datetime
// (chrono's "%Y-%m-%d %H:%M:%S %:z"). Unparseable input passes through.
function localDatetime(rfc3339) {
  const t = new Date(rfc3339);
  if (Number.isNaN(t.getTime())) return rfc3339;
  const p = (n) => String(n).padStart(2, "0");
  const off = -t.getTimezoneOffset();
  const sign = off < 0 ? "-" : "+";
  const abs = Math.abs(off);
  return (
    `${t.getFullYear()}-${p(t.getMonth() + 1)}-${p(t.getDate())} ` +
    `${p(t.getHours())}:${p(t.getMinutes())}:${p(t.getSeconds())} ` +
    `${sign}${p(Math.floor(abs / 60))}:${p(abs % 60)}`
  );
}

// Compact relative time ("now", "5m", "3h", "2d", "3mo", "1y"), a port of
// glauca_core::time::humanize_secs (same buckets, negative clamps to "now").
function relativeTime(rfc3339, nowMs = Date.now()) {
  const t = Date.parse(rfc3339);
  if (Number.isNaN(t)) return rfc3339;
  const secs = Math.floor((nowMs - t) / 1000);
  const MIN = 60;
  const HOUR = 60 * MIN;
  const DAY = 24 * HOUR;
  const MONTH = 30 * DAY;
  const YEAR = 365 * DAY;
  if (secs < MIN) return "now";
  if (secs < HOUR) return `${Math.floor(secs / MIN)}m`;
  if (secs < DAY) return `${Math.floor(secs / HOUR)}h`;
  if (secs < MONTH) return `${Math.floor(secs / DAY)}d`;
  if (secs < YEAR) return `${Math.floor(secs / MONTH)}mo`;
  return `${Math.floor(secs / YEAR)}y`;
}

// Build one item row, mirroring the GUI's render_item_row: (1) state octicon
// beside a wrapping bold title, (2) participants — author → assignees left,
// reviewers + comment count right, (3) meta — lock, owner/name#num · labels,
// relative update time. Unread rows get a faint background tint (is_new);
// selection takes precedence.
function renderItemList() {
  const list = $("item-list");
  list.replaceChildren();
  const e = state.entries[state.selectedEntry];
  $("items-title").textContent = e ? e.label : "Items";
  const now = Date.now(); // sample the clock once for all rows
  state.visibleItems.forEach((it, idx) => {
    const key = itemKey(it);
    const selected = key === state.selectedItemKey;

    const icon = itemStateIcon(it);
    const rows = [
      el("div", { class: "it-title-row" }, [
        octicon(icon.name, `it-state ${icon.cls}`, 16),
        renderHighlightedTitle(it.title, state.visibleTitleSegments[idx]),
      ]),
    ];

    const assignees = it.assignees || [];
    const overlays = reviewerOverlays(it);
    const hasParticipants = it.author || assignees.length || overlays.length || it.comment_count > 0;
    if (hasParticipants) {
      const left = el("div", { class: "it-people-side" });
      if (it.author) left.appendChild(avatarEl(it.author));
      // Arrow reads "author → assignee(s)" when both sides exist.
      if (it.author && assignees.length) left.appendChild(el("span", { class: "people-extra", text: "→" }));
      for (const u of assignees.slice(0, AVATAR_LIMIT)) left.appendChild(avatarEl(u));
      if (assignees.length > AVATAR_LIMIT)
        left.appendChild(el("span", { class: "people-extra", text: `+${assignees.length - AVATAR_LIMIT}` }));

      const right = el("div", { class: "it-people-side" });
      for (const o of overlays.slice(0, AVATAR_LIMIT)) right.appendChild(reviewerAvatar(o.user, o.state));
      if (overlays.length > AVATAR_LIMIT)
        right.appendChild(el("span", { class: "people-extra", text: `+${overlays.length - AVATAR_LIMIT}` }));
      if (it.comment_count > 0) {
        right.appendChild(
          el("span", { class: "comment-count" }, [octicon("comment", "muted", 12), el("span", { text: String(it.comment_count) })])
        );
      }
      rows.push(el("div", { class: "it-people" }, [left, right]));
    }

    let metaText = `${it.repo_owner}/${it.repo_name}#${it.number}`;
    if ((it.labels || []).length) metaText += ` · ${it.labels.join(", ")}`;
    rows.push(
      el("div", { class: "it-meta" }, [
        it.repo_private ? octicon("lock", "muted", 12) : null,
        el("span", { class: "meta-text", text: metaText }),
        el("span", { class: "meta-time", text: relativeTime(it.updated_at, now) }),
      ])
    );

    const li = el(
      "li",
      {
        class: [selected ? "selected" : "", !selected && it.is_new ? "unread" : ""].filter(Boolean).join(" "),
        // Shift+click also opens the row in the browser (mouse-only equivalent
        // of the `o` key), like the GUI.
        onclick: (ev) => {
          selectItem(it);
          if (ev.shiftKey) call("open_browser", { item: it });
        },
      },
      rows
    );
    li.addEventListener("contextmenu", (ev) => {
      ev.preventDefault();
      selectItem(it);
      showItemActionMenu(it, ev.clientX, ev.clientY);
    });
    list.appendChild(li);
  });
}

// Reset the detail pane to its "nothing selected" state. The emptied header
// hides itself via the #detail-header:empty CSS rule.
function clearDetail() {
  $("detail-header").replaceChildren();
  const scroll = $("detail-scroll");
  scroll.className = "";
  scroll.replaceChildren(el("div", { class: "detail-empty", text: "Select an item" }));
}

// Octicon name, color class, and tooltip label for a PR's reviewDecision
// (mirrors the GUI's review_decision_icon).
function reviewDecisionInfo(decision) {
  if (decision === "APPROVED") return ["check-circle-fill", "state-open", "Approved"];
  if (decision === "CHANGES_REQUESTED") return ["x-circle-fill", "state-closed", "Changes requested"];
  if (decision === "REVIEW_REQUIRED") return ["clock", "state-pending", "Review required"];
  return ["comment", "muted", "Review"];
}

// Review-decision octicon for the detail header; the wrapping span carries
// the tooltip.
function reviewDecisionIcon(decision) {
  const [name, cls, label] = reviewDecisionInfo(decision);
  const span = el("span", { class: "decision-icon" }, [octicon(name, cls, 20)]);
  span.title = label;
  return span;
}

// Detail pane, mirroring the GUI's render_detail: a pinned header (title line
// with state pill, then `label: value` field rows, then the action buttons)
// above a scrollable plain-text body. Markdown rendering is a separate task.
function renderDetail(it) {
  const header = $("detail-header");
  const scroll = $("detail-scroll");
  header.replaceChildren();
  scroll.scrollTop = 0; // new item shown → back to the top, mirroring the TUI/GUI

  // Title line: author avatar + state pill + (PR) review-decision icon + the
  // wrapping bold title.
  const icon = itemStateIcon(it);
  const pill = el("span", { class: `state-pill ${icon.cls}` }, [
    octicon(icon.name, "", 12),
    el("span", { text: stateLabel(it) }),
  ]);
  header.appendChild(
    el("div", { class: "detail-title-row" }, [
      el("span", { class: "detail-lead" }, [
        it.author ? avatarEl(it.author) : null,
        pill,
        it.review_decision ? reviewDecisionIcon(it.review_decision) : null,
      ]),
      el("span", { class: "detail-title", text: it.title }),
    ])
  );

  // `label: value` field rows (the GUI's detail_field / detail_people_field).
  const field = (label, value) =>
    el("div", { class: "field" }, [el("span", { class: "field-label", text: label }), value]);
  const textField = (label, text) => field(label, el("span", { class: "field-value", text }));
  const userChip = (u) => el("span", { class: "chip" }, [avatarEl(u), el("span", { text: u.login })]);
  const reviewerChip = (o) => el("span", { class: "chip" }, [reviewerAvatar(o.user, o.state), el("span", { text: o.user.login })]);

  if ((it.labels || []).length) header.appendChild(textField("labels", it.labels.join(", ")));
  if (it.base_ref && it.head_ref) header.appendChild(textField("branch", `${it.head_ref} → ${it.base_ref}`));
  const assignees = it.assignees || [];
  if (assignees.length)
    header.appendChild(field("assignees", el("span", { class: "field-chips" }, assignees.map(userChip))));
  const overlays = reviewerOverlays(it);
  if (overlays.length)
    header.appendChild(field("reviewers", el("span", { class: "field-chips" }, overlays.map(reviewerChip))));
  if (it.milestone) header.appendChild(textField("milestone", it.milestone));
  if (it.created_at_item) header.appendChild(textField("created", localDatetime(it.created_at_item)));
  header.appendChild(textField("updated", localDatetime(it.updated_at)));

  // Actions. The engine performs the external work (gh CLI / browser); the
  // front-end only sends the command. The GUI drives these solely from the
  // item menu; the buttons stay here as a mouse convenience.
  const actions = el("div", { class: "actions" });
  for (const a of itemActions(it)) {
    actions.appendChild(el("button", { text: a.label, onclick: a.run }));
  }
  header.appendChild(actions);

  // Custom actions (actions.toml): appended asynchronously and only when at
  // least one action applies to this kind, mirroring the GUI's conditional
  // "Custom actions" submenu. Definitions stay Rust-side; JS only sees
  // {name, label} and runs by name.
  invoke("list_custom_actions", { kind: it.kind })
    .then((acts) => {
      if (!acts.length) return;
      actions.appendChild(
        el("button", {
          text: "Custom actions…",
          onclick: (ev) => showCustomActionsMenu(it, acts, ev.clientX, ev.clientY),
        })
      );
    })
    .catch((e) => setStatus(`custom actions: ${e}`, true));

  scroll.className = "with-header";
  scroll.replaceChildren(
    it.body && it.body.trim().length
      ? el("div", { class: "body", text: it.body })
      : el("div", { class: "body-empty", text: "(no description)" })
  );
}

// The actions applicable to `it`, shared by the detail-pane buttons and the
// item action menu (Enter / right-click). Mirrors ItemAction::available_for.
function itemActions(it) {
  const entry = state.entries[state.selectedEntry];
  const acts = [
    { label: "Open in browser", run: () => call("open_browser", { item: it }) },
    { label: "Copy URL", run: () => copyText(it.url) },
    {
      label: "Refresh",
      run: () =>
        call("refresh_item", { queryId: entry.rootQueryId, repoOwner: it.repo_owner, repoName: it.repo_name, number: it.number }),
    },
    { label: "View comments", run: () => call("load_comments", { owner: it.repo_owner, repo: it.repo_name, number: it.number }) },
    {
      label: "Comment",
      run: async () => {
        const text = await promptModal("Comment body:");
        if (text) call("comment", { url: it.url, kind: it.kind, body: text });
      },
    },
  ];
  if (it.kind === "pull_request") {
    acts.push({ label: "Approve", run: () => call("submit_review", { url: it.url, event: "approve", body: null }) });
    acts.push({
      label: "Request changes",
      run: async () => {
        const text = await promptModal("Request changes — comment:");
        if (text) call("submit_review", { url: it.url, event: "request_changes", body: text });
      },
    });
    acts.push({
      label: "Review comment",
      run: async () => {
        const text = await promptModal("Review comment:");
        if (text) call("submit_review", { url: it.url, event: "comment", body: text });
      },
    });
    acts.push({
      label: "Merge",
      run: async () => {
        const s = ((await promptModal("Merge strategy (squash / merge / rebase):", "squash")) || "").trim();
        if (["squash", "merge", "rebase"].includes(s)) call("merge", { url: it.url, strategy: s });
      },
    });
  }
  return acts;
}

// Picker over the given custom actions for `it` (from list_custom_actions).
// The result surfaces through the usual ActionDone / ActionError messages.
function showCustomActionsMenu(it, acts, x, y) {
  showContextMenu(
    x,
    y,
    acts.map((a) => ({
      label: a.label,
      onClick: () => call("run_custom_action", { name: a.name, item: it }),
    }))
  );
}

// Full action menu for an item (Enter key / right-click on a row): the shared
// item actions plus any custom actions after a separator.
async function showItemActionMenu(it, x, y) {
  const menu = itemActions(it).map((a) => ({ label: a.label, onClick: a.run }));
  let acts = [];
  try {
    acts = await invoke("list_custom_actions", { kind: it.kind });
  } catch {
    /* menu is still useful without custom actions */
  }
  if (acts.length) {
    menu.push(null);
    for (const a of acts) {
      menu.push({ label: a.label, onClick: () => call("run_custom_action", { name: a.name, item: it }) });
    }
  }
  showContextMenu(x, y, menu);
}

// Open the comments overlay for the loaded comments (set by CommentsLoaded).
// Supports toggling minimized comments and oldest/newest sort.
function openCommentsModal() {
  document.querySelectorAll(".modal-overlay").forEach((m) => m.remove());
  commentsCtl = null; // any previous modal is gone
  const overlay = el("div", { class: "modal-overlay" });
  const listBox = el("div", { class: "comments-list" });

  const render = () => {
    listBox.replaceChildren();
    let comments = state.comments.slice();
    if (!state.commentsShowMinimized) comments = comments.filter((c) => !c.is_minimized);
    if (state.commentsSortNewest) comments.reverse();
    if (!comments.length) {
      listBox.appendChild(el("div", { class: "chead", text: "No comments." }));
      return;
    }
    for (const c of comments) {
      const head = c.is_minimized
        ? `${c.author} · ${c.created_at} · minimized${c.minimized_reason ? ` (${c.minimized_reason})` : ""}`
        : `${c.author} · ${c.created_at}`;
      listBox.appendChild(
        el("div", { class: "comment" }, [el("div", { class: "chead", text: head }), el("div", { class: "cbody", text: c.body })])
      );
    }
  };

  const minToggle = el("button", {
    text: `Minimized: ${state.commentsShowMinimized ? "shown" : "hidden"}`,
    onclick: () => {
      state.commentsShowMinimized = !state.commentsShowMinimized;
      minToggle.textContent = `Minimized: ${state.commentsShowMinimized ? "shown" : "hidden"}`;
      render();
    },
  });
  const sortToggle = el("button", {
    text: `Sort: ${state.commentsSortNewest ? "newest" : "oldest"}`,
    onclick: () => {
      state.commentsSortNewest = !state.commentsSortNewest;
      sortToggle.textContent = `Sort: ${state.commentsSortNewest ? "newest" : "oldest"}`;
      render();
    },
  });
  const closeOverlay = () => {
    commentsCtl = null;
    overlay.remove();
  };
  const close = el("button", { text: "Close", onclick: closeOverlay });
  const header = el("div", { class: "comments-header" }, [
    el("span", { class: "modal-title", text: `Comments (${state.comments.length})` }),
    minToggle,
    sortToggle,
    close,
  ]);

  render();
  overlay.appendChild(el("div", { class: "modal-box comments-box" }, [header, listBox]));
  document.body.appendChild(overlay);

  // Hand the global keydown handler a controller for j/k/g/G/s/h/q (see
  // handleCommentsKey), matching the TUI's comments-popup keys.
  commentsCtl = {
    scroll: (dy) => {
      listBox.scrollTop += dy;
    },
    top: () => {
      listBox.scrollTop = 0;
    },
    bottom: () => {
      listBox.scrollTop = listBox.scrollHeight;
    },
    toggleSort: () => sortToggle.click(),
    toggleMin: () => minToggle.click(),
    close: closeOverlay,
  };
}

// ── actions ──────────────────────────────────────────────────────────────────

// Preview an entry (j/k cursor move): cached items only — no sync, so scrolling
// through the list never hits the network (mirrors the GUI's preview_entry).
// Selecting also does NOT mark anything read: unread is per-item and clears as
// items are read (selectItem).
function previewEntry(idx) {
  state.selectedEntry = idx;
  state.selectedItemKey = null;
  const e = state.entries[idx];
  call("load_cached", { queryId: e.rootQueryId });
  renderSidebar();
  refreshVisible();
  updateBanner();
  clearDetail();
}

// Commit to an entry (click / Enter): preview plus a sync of the backing root
// query (mirrors the GUI's select_index with always_sync).
function selectEntry(idx) {
  previewEntry(idx);
  setFocus("entries");
  const e = state.entries[idx];
  if (!e.isFilterStream) {
    call("sync", { queryId: e.rootQueryId, queryStr: e.queryStr });
  }
}

function selectItem(it) {
  state.selectedItemKey = itemKey(it);
  setFocus("items");
  // Mirrors the GUI's mark_current_item_read: persist via the engine, then
  // advance the local copy so unread badges/row styling react immediately
  // without waiting for a reload (the DB write does the same server-side).
  if (isUnread(it)) {
    const e = state.entries[state.selectedEntry];
    call("mark_item_read", { queryId: e.rootQueryId, repoOwner: it.repo_owner, repoName: it.repo_name, number: it.number });
    it.last_read_updated_at = it.updated_at;
    it.is_new = false;
    refreshUnread(e.rootQueryId); // recompute badges (also re-renders the sidebar)
  }
  // Cache maintenance clears the re-fetchable `body` of old items to save space;
  // a null/undefined body means "cleared" (a genuinely empty description is "").
  // Re-fetch it once on open — the resulting ItemsLoaded re-renders the detail.
  if (it.body == null && !state.bodyRefreshRequested.has(state.selectedItemKey)) {
    const e = state.entries[state.selectedEntry];
    if (e) {
      state.bodyRefreshRequested.add(state.selectedItemKey);
      call("refresh_item", { queryId: e.rootQueryId, repoOwner: it.repo_owner, repoName: it.repo_name, number: it.number });
    }
  }
  renderItemList(); // selection/read changed; the filtered set is unchanged
  renderDetail(it);
}

// ── entry management (CRUD / reorder / mark all read) ─────────────────────────--

// Rebuild the left pane from the DB after a structural change, preserving the
// selection by id (the engine confirms add/edit/delete/swap with a message; we
// re-fetch rather than reorder in JS — list_entries is the source of truth).
async function refreshEntries() {
  const prev = state.entries[state.selectedEntry];
  const prevKey = prev ? unreadKey(prev.isFilterStream, prev.id) : null;
  try {
    const raw = await invoke("list_entries");
    state.rawEntries = raw;
    state.entries = raw.map(normalize);
  } catch (e) {
    setStatus(`entries: ${e}`, true);
    return;
  }
  const idx = prevKey ? state.entries.findIndex((e) => unreadKey(e.isFilterStream, e.id) === prevKey) : -1;
  if (idx >= 0) {
    state.selectedEntry = idx;
    renderSidebar();
  } else if (state.entries.length) {
    selectEntry(0); // previous selection was deleted
  } else {
    state.selectedEntry = -1;
    state.visibleItems = [];
    renderSidebar();
    renderItemList();
  }
}

// Build the SwapQuery/SwapFilterStream args for moving entry `idx` up/down,
// mirroring the TUI's reorder_command. Returns {cmd, args} or null at an edge.
function reorderArgs(idx, down) {
  const e = state.entries[idx];
  if (!e) return null;
  if (!e.isFilterStream) {
    if (down) {
      let j = idx + 1;
      while (j < state.entries.length && state.entries[j].isFilterStream) j++;
      const next = state.entries[j];
      if (next && !next.isFilterStream)
        return { cmd: "swap_query_positions", args: { upperId: e.id, lowerId: next.id, activeId: e.id } };
    } else {
      let j = idx - 1;
      while (j >= 0 && state.entries[j].isFilterStream) j--;
      const prev = state.entries[j];
      if (prev && !prev.isFilterStream)
        return { cmd: "swap_query_positions", args: { upperId: prev.id, lowerId: e.id, activeId: e.id } };
    }
  } else if (down) {
    const next = state.entries[idx + 1];
    if (next && next.isFilterStream && next.rootQueryId === e.rootQueryId)
      return { cmd: "swap_filter_stream_positions", args: { upperId: e.id, lowerId: next.id, activeId: e.id } };
  } else {
    const prev = state.entries[idx - 1];
    if (prev && prev.isFilterStream && prev.rootQueryId === e.rootQueryId)
      return { cmd: "swap_filter_stream_positions", args: { upperId: prev.id, lowerId: e.id, activeId: e.id } };
  }
  return null;
}

async function newQuery() {
  const out = await formModal("New query", [
    { key: "name", label: "Name (optional)" },
    { key: "query", label: "GitHub search query", required: true },
  ]);
  if (out) call("add_query", { name: out.name || null, query: out.query });
}

async function editQuery(e) {
  const out = await formModal("Edit query", [
    { key: "name", label: "Name (optional)", value: e.label },
    { key: "query", label: "GitHub search query", value: e.queryStr, required: true },
  ]);
  if (out) call("edit_query", { id: e.id, name: out.name || null, query: out.query });
}

async function newFilterStream(parent) {
  const out = await filterStreamModal(`New filter stream under "${parent.label}"`, "", [""]);
  if (out) call("add_filter_stream", { parentId: parent.rootQueryId, kind: parent.kind, name: out.name, filter: out.filter });
}

async function editFilterStream(e) {
  const out = await filterStreamModal("Edit filter stream", e.label, splitFilterGroups(e.streamFilter));
  if (out) call("edit_filter_stream", { id: e.id, name: out.name, filter: out.filter });
}

async function deleteEntry(e) {
  const what = e.isFilterStream ? "filter stream" : "query (and its filter streams)";
  if (!(await confirmModal(`Delete this ${what}: "${e.label}"?`))) return;
  if (e.isFilterStream) call("delete_filter_stream", { id: e.id });
  else call("delete_query", { queryId: e.id });
}

function moveEntry(idx, down) {
  const r = reorderArgs(idx, down);
  if (r) call(r.cmd, r.args);
}

function markAllRead(e) {
  call("mark_all_read", { queryId: e.rootQueryId, filter: e.isFilterStream ? e.streamFilter : null });
}

// Context menu for a left-pane entry.
function entryMenu(ev, idx) {
  ev.preventDefault();
  const e = state.entries[idx];
  const items = [];
  if (!e.isFilterStream) items.push({ label: "New filter stream", onClick: () => newFilterStream(e) });
  items.push({ label: "Edit", onClick: () => (e.isFilterStream ? editFilterStream(e) : editQuery(e)) });
  items.push({ label: "Delete", onClick: () => deleteEntry(e) });
  items.push(null);
  items.push({ label: "Move up", onClick: () => moveEntry(idx, false) });
  items.push({ label: "Move down", onClick: () => moveEntry(idx, true) });
  items.push(null);
  items.push({ label: "Mark all read", onClick: () => markAllRead(e) });
  if (!e.isFilterStream) {
    items.push({ label: "Full resync", onClick: () => fullResyncEntry(e) });
  }
  showContextMenu(ev.clientX, ev.clientY, items);
}

// ── keyboard navigation ──────────────────────────────────────────────────────
//
// One document-level keydown handler, dispatched by context (mirrors the GUI's
// keybinding contexts: GLAUCA_CONTEXT excludes Input / comments / menus). All
// bindings follow the TUI/GUI keymap; `?` shows the reference.

function setFocus(f) {
  state.focus = f;
  $("sidebar").classList.toggle("focused", f === "entries");
  $("items").classList.toggle("focused", f === "items");
  $("detail").classList.toggle("focused", f === "detail");
}

// Pixels scrolled per j/k in the detail pane (matches the comments overlay step).
const DETAIL_SCROLL_STEP = 60;

// Scroll the detail body when the detail pane is focused. The browser clamps
// scrollTop to [0, scrollHeight - clientHeight], so no bounds math is needed.
function scrollDetail(dy) {
  $("detail-scroll").scrollTop += dy;
}

// Which key context the event belongs to. Modals and menus own their keys;
// text inputs get everything except Escape-to-blur; the rest is navigation.
function keyContext(ev) {
  if (document.querySelector(".ctx-menu")) return "menu";
  const overlay = document.querySelector(".modal-overlay");
  if (overlay) return overlay.querySelector(".comments-box") ? "comments" : "modal";
  const t = ev.target;
  if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.tagName === "SELECT")) return "input";
  return "nav";
}

// Scroll the selected row of a list pane into view after a key-driven move.
function revealSelected(selector) {
  const li = document.querySelector(selector);
  if (li) li.scrollIntoView({ block: "nearest" });
}

function moveEntryCursor(delta) {
  if (!state.entries.length) return;
  const next = Math.max(0, Math.min(state.entries.length - 1, state.selectedEntry + delta));
  if (next === state.selectedEntry) return;
  previewEntry(next);
  revealSelected("#entries li.selected");
}

function moveItemCursor(delta) {
  const list = state.visibleItems;
  if (!list.length) return;
  const idx = list.findIndex((x) => itemKey(x) === state.selectedItemKey);
  const next = idx < 0 ? 0 : Math.max(0, Math.min(list.length - 1, idx + delta));
  selectItem(list[next]);
  revealSelected("#item-list li.selected");
}

// Anchor point for keyboard-opened menus: next to the selected item row.
function selectedItemAnchor() {
  const li = document.querySelector("#item-list li.selected");
  if (!li) return { x: Math.round(window.innerWidth / 3), y: Math.round(window.innerHeight / 3) };
  const r = li.getBoundingClientRect();
  return { x: Math.round(r.left + 24), y: Math.round(r.bottom - 2) };
}

// The query string backing `e`'s root query (a stream carries no query of its own).
function rootQueryStr(e) {
  if (!e.isFilterStream) return e.queryStr;
  const root = state.entries.find((x) => !x.isFilterStream && x.id === e.rootQueryId);
  return root ? root.queryStr : null;
}

// Sync / full-resync the entry's backing root query — the single dispatch
// point shared by the Glauca menu, the r/S keys, and the entry context menu.
// No-ops when there is no entry or its root query can't be resolved.
function syncEntry(e) {
  const q = e && rootQueryStr(e);
  if (q) call("sync", { queryId: e.rootQueryId, queryStr: q });
}

function fullResyncEntry(e) {
  const q = e && rootQueryStr(e);
  if (q) call("full_resync", { queryId: e.rootQueryId, queryStr: q });
}

// Comments-modal keyboard controller, installed by openCommentsModal while the
// modal is open (null otherwise). Keeps the global handler decoupled from the
// modal's DOM internals.
let commentsCtl = null;

function handleCommentsKey(ev) {
  if (!commentsCtl) return;
  const handled = () => ev.preventDefault();
  switch (ev.key) {
    case "j":
    case "ArrowDown":
      commentsCtl.scroll(60);
      return handled();
    case "k":
    case "ArrowUp":
      commentsCtl.scroll(-60);
      return handled();
    case "g":
      commentsCtl.top();
      return handled();
    case "G":
      commentsCtl.bottom();
      return handled();
    case "s":
      commentsCtl.toggleSort();
      return handled();
    case "h":
      commentsCtl.toggleMin();
      return handled();
    case "q":
    case "Escape":
      commentsCtl.close();
      return handled();
    default:
      return undefined;
  }
}

function handleNavKey(ev) {
  const e = state.entries[state.selectedEntry];
  const it = state.visibleItems.find((x) => itemKey(x) === state.selectedItemKey) || null;
  const handled = () => ev.preventDefault();
  switch (ev.key) {
    case "j":
    case "ArrowDown":
      if (state.focus === "entries") moveEntryCursor(1);
      else if (state.focus === "detail") scrollDetail(DETAIL_SCROLL_STEP);
      else moveItemCursor(1);
      return handled();
    case "k":
    case "ArrowUp":
      if (state.focus === "entries") moveEntryCursor(-1);
      else if (state.focus === "detail") scrollDetail(-DETAIL_SCROLL_STEP);
      else moveItemCursor(-1);
      return handled();
    // h/l cycle the three panes (entries → items → detail → entries), matching the
    // TUI/GUI. In the detail pane j/k scroll the body (see above).
    case "h":
    case "ArrowLeft":
      setFocus(state.focus === "entries" ? "detail" : state.focus === "items" ? "entries" : "items");
      return handled();
    case "l":
    case "ArrowRight":
      setFocus(state.focus === "entries" ? "items" : state.focus === "items" ? "detail" : "entries");
      return handled();
    case "Enter":
      // A focused button (e.g. a detail-pane action) keeps its native Enter.
      if (ev.target && ev.target.tagName === "BUTTON") return undefined;
      if (state.focus === "entries") {
        if (e) selectEntry(state.selectedEntry);
      } else if (it) {
        const a = selectedItemAnchor();
        showItemActionMenu(it, a.x, a.y);
      }
      return handled();
    case "/":
      $("filter").focus();
      return handled();
    // Entry CRUD keys act on the selected left-pane entry, so — like the TUI/GUI,
    // which gate these to Focus::QueryList — they only fire when the entries pane
    // is focused. Otherwise pressing e/d/a while reading an item would edit,
    // delete, or mark-all-read the selected query out from under the reader.
    case "n":
      if (state.focus === "entries") newQuery();
      return handled();
    case "f":
      if (state.focus === "entries" && e) newFilterStream(e);
      return handled();
    case "e":
      if (state.focus === "entries" && e)
        e.isFilterStream ? editFilterStream(e) : editQuery(e);
      return handled();
    case "d":
      if (state.focus === "entries" && e) deleteEntry(e);
      return handled();
    case "a":
      if (state.focus === "entries" && e) markAllRead(e);
      return handled();
    case "J":
      if (state.focus === "entries") moveEntry(state.selectedEntry, true);
      return handled();
    case "K":
      if (state.focus === "entries") moveEntry(state.selectedEntry, false);
      return handled();
    case "o":
      if (it) call("open_browser", { item: it });
      return handled();
    case "y":
      if (it) copyText(it.url);
      return handled();
    case "c":
      if (it) call("load_comments", { owner: it.repo_owner, repo: it.repo_name, number: it.number });
      return handled();
    case "x":
      if (it) {
        invoke("list_custom_actions", { kind: it.kind })
          .then((acts) => {
            if (!acts.length) {
              setStatus("No custom actions for this item");
              return;
            }
            const a = selectedItemAnchor();
            showCustomActionsMenu(it, acts, a.x, a.y);
          })
          .catch((err) => setStatus(`custom actions: ${err}`, true));
      }
      return handled();
    case "r":
      // Context-dependent like the TUI: entries → re-sync the selected entry's
      // root query, items → re-fetch the selected item.
      if (state.focus === "entries") {
        syncEntry(e);
      } else if (it && e) {
        call("refresh_item", { queryId: e.rootQueryId, repoOwner: it.repo_owner, repoName: it.repo_name, number: it.number });
      }
      return handled();
    case "S":
      fullResyncEntry(e);
      return handled();
    case "u":
      if (e) applyPending(e.rootQueryId);
      return handled();
    case "?":
      openHelpModal();
      return handled();
    default:
      return undefined;
  }
}

function onKeyDown(ev) {
  if (ev.ctrlKey || ev.metaKey || ev.altKey) return;
  switch (keyContext(ev)) {
    case "menu": // the context menu's own capture handler owns Escape/clicks
    case "modal": // form/settings modals handle Enter/Escape on their inputs
      return;
    case "input":
      if (ev.key === "Escape") ev.target.blur();
      return;
    case "comments":
      handleCommentsKey(ev);
      return;
    default:
      handleNavKey(ev);
  }
}

const KEY_HELP = [
  ["j / k / ↓ / ↑", "Move selection (entries preview without syncing)"],
  ["h / l / ← / →", "Focus the entries / items pane"],
  ["Enter", "Entries: select & sync · Items: action menu"],
  ["/", "Focus the filter box (Enter/Esc to leave)"],
  ["n", "New query"],
  ["f", "New filter stream under the selected query"],
  ["e", "Edit the selected entry"],
  ["d", "Delete the selected entry"],
  ["a", "Mark all read in the selected entry"],
  ["Shift+J / Shift+K", "Reorder the selected entry"],
  ["o", "Open the selected item in the browser"],
  ["y", "Copy the selected item's URL"],
  ["c", "View comments"],
  ["x", "Custom actions"],
  ["r", "Entries: re-sync · Items: refresh the item"],
  ["S", "Full resync of the selected root query"],
  ["u", "Apply pending background updates"],
  ["?", "This help"],
  ["Comments: j/k g/G", "Scroll · jump to top/bottom"],
  ["Comments: s / h / q", "Toggle sort · toggle minimized · close"],
];

function openHelpModal() {
  const rows = KEY_HELP.map(([keys, desc]) =>
    el("div", { class: "help-row" }, [el("span", { class: "help-keys", text: keys }), el("span", { text: desc })])
  );
  infoModal("Keyboard shortcuts", rows, { closeKeys: ["Escape", "q", "?"], boxClass: "help-box" });
}

// ── engine messages ────────────────────────────────────────────────────────--

function handleMessage(msg) {
  const d = msg.data;
  switch (msg.type) {
    case "ItemsLoaded": {
      const e = state.entries[state.selectedEntry];
      const isCurrent = e && e.rootQueryId === d.query_id;
      if (d.background && isCurrent) {
        // Engine contract: defer background-sync results for the query the user
        // is viewing so the list doesn't reshuffle under them. Hold them back and
        // surface a banner; the user applies them on click (see updateBanner).
        state.pending.set(d.query_id, d.items);
        updateBanner();
        break;
      }
      state.itemsByQuery.set(d.query_id, d.items);
      state.pending.delete(d.query_id); // a fresh foreground load supersedes any held-back items
      if (isCurrent) {
        // refreshVisible also re-renders the open detail from the reloaded items,
        // so a transparently re-fetched body (see selectItem) becomes visible.
        refreshVisible();
        updateBanner();
      }
      refreshUnread(d.query_id);
      break;
    }
    case "CommentsLoaded":
      state.comments = d;
      openCommentsModal();
      break;
    case "CommentsFailed":
      setStatus(`comments: ${d}`, true);
      break;
    case "Status":
      setStatus(d);
      break;
    case "ActionDone":
      setStatus(d);
      break;
    case "ActionError":
      setStatus(d, true);
      break;
    case "SyncStarted":
      state.syncingCount += 1;
      setStatus(`syncing #${d.query_id}…`);
      break;
    case "SyncDone":
      state.syncingCount = Math.max(0, state.syncingCount - 1);
      setStatus(`synced ${d.count} item(s)`);
      break;
    case "SyncError":
      state.syncingCount = Math.max(0, state.syncingCount - 1);
      setStatus(`sync error: ${d.error}`, true);
      break;
    // Background-sync queue depth, shown as "N bg" in the sidebar footer (the
    // GUI's bg_sync_pending counter).
    case "BgSyncQueued":
      state.bgSyncPending += d;
      renderFooter();
      break;
    case "BgSyncJobDone":
      state.bgSyncPending = Math.max(0, state.bgSyncPending - 1);
      renderFooter();
      break;
    // Structural changes: the engine confirms with these; rebuild the left pane
    // from the DB (list_entries) rather than reordering in JS.
    case "QueryAdded":
    case "FilterStreamAdded":
    case "QueryUpdated":
    case "FilterStreamUpdated":
    case "QueryDeleted":
    case "FilterStreamDeleted":
    case "QueriesSwapped":
    case "FilterStreamsSwapped":
      refreshEntries();
      break;
    default:
      break;
  }
}

// ── settings & theme ──────────────────────────────────────────────────────--

// Apply the theme to <html>: theme-light / theme-dark / theme-system. Under
// "system", a `sys-light` class tracks the OS color scheme (see CSS).
function applyTheme(theme) {
  const html = document.documentElement;
  html.className = `theme-${theme}`;
  if (theme === "system") {
    const light = window.matchMedia && window.matchMedia("(prefers-color-scheme: light)").matches;
    html.classList.toggle("sys-light", !!light);
  }
}

// Persist the full settings object (the TOML holds all fields, so partial
// updates spread over the current state) and adopt it locally on success. The
// object passes through to Rust's TauriSettings as-is — no per-field mapping
// to keep in sync when settings grow.
function persistSettings(next) {
  return invoke("save_settings", { settings: next })
    .then(() => {
      state.settings = next;
    })
    .catch((e) => setStatus(`settings: ${e}`, true));
}

function setTheme(theme) {
  applyTheme(theme);
  persistSettings({ ...state.settings, theme });
}

// Settings modal (Glauca > Settings…). Theme and notifications moved to the
// View menu, matching the GUI; only the sync interval — which the GUI has no
// menu equivalent for — remains here, so the generic form modal covers it.
async function openSettingsModal() {
  const s = state.settings;
  const out = await formModal("Settings", [
    { key: "secs", label: "Sync interval (seconds)", value: String(s.sync_interval_secs) },
  ]);
  if (!out) return;
  await persistSettings({ ...s, sync_interval_secs: Math.max(10, parseInt(out.secs, 10) || 60) });
  setStatus("settings saved (sync interval applies on restart)");
}

// ── bootstrap ──────────────────────────────────────────────────────────────--

async function main() {
  // Debounce filtering so typing fast in a large list doesn't round-trip to the
  // engine on every keystroke (mirrors the GUI's FILTER_DEBOUNCE).
  let filterTimer = null;
  $("filter").addEventListener("input", (ev) => {
    state.filterText = ev.target.value;
    if (filterTimer !== null) clearTimeout(filterTimer);
    filterTimer = setTimeout(() => {
      filterTimer = null;
      refreshVisible();
    }, 150);
  });
  // Leave the filter box with Enter/Escape and land on the item list, so `/` →
  // type → Enter → j/k flows without the mouse.
  $("filter").addEventListener("keydown", (ev) => {
    if (ev.key === "Enter" || ev.key === "Escape") {
      ev.target.blur();
      setFocus("items");
    }
  });

  document.addEventListener("keydown", onKeyDown);
  setFocus("entries");

  // Safety net: anything that still slips through as an unhandled rejection
  // (e.g. a future missed await) lands on the sidebar footer, not the void.
  window.addEventListener("unhandledrejection", (ev) => setStatus(String(ev.reason), true));

  wireMenuButton("menu-glauca", glaucaMenuItems);
  wireMenuButton("menu-view", viewMenuItems);
  wireMenuButton("menu-help", helpMenuItems);

  // Right-click on the entry list's empty space → New query (the GUI's
  // NewQueryOnly menu). Rows handle their own context menu (entryMenu).
  $("entries").addEventListener("contextmenu", (ev) => {
    if (ev.target.closest("li")) return;
    ev.preventDefault();
    showContextMenu(ev.clientX, ev.clientY, [{ label: "New query", onClick: newQuery }]);
  });
  // Right-click anywhere in the detail pane → the selected item's action menu.
  $("detail").addEventListener("contextmenu", (ev) => {
    ev.preventDefault();
    const it = state.visibleItems.find((x) => itemKey(x) === state.selectedItemKey);
    if (it) showItemActionMenu(it, ev.clientX, ev.clientY);
  });

  // Load + apply persisted settings (theme, pane widths) before anything renders.
  try {
    state.settings = await invoke("get_settings");
  } catch {
    /* defaults */
  }
  applyTheme(state.settings.theme);
  applyPaneSizes(state.settings.pane_sizes);
  setupPaneResize();
  if (window.matchMedia) {
    window.matchMedia("(prefers-color-scheme: light)").addEventListener("change", () => {
      if (state.settings.theme === "system") applyTheme("system");
    });
  }

  await listen("app-message", (event) => handleMessage(event.payload));

  const init = await invoke("init");
  state.currentUser = init.current_user;
  state.rawEntries = init.entries;
  state.entries = init.entries.map(normalize);
  renderSidebar();

  // Sidebar header: signed-in user (36px avatar + login + display name),
  // mirroring the GUI's left-pane header.
  const header = $("sidebar-header");
  if (init.current_user) {
    header.replaceChildren(
      avatarEl({ login: init.current_user, avatar_url: init.current_user_avatar_url }, "avatar lg", 36),
      el("div", { class: "who" }, [
        el("div", { class: "login", text: `@${init.current_user}` }),
        init.current_user_name ? el("div", { class: "name", text: init.current_user_name }) : null,
      ])
    );
  } else {
    header.replaceChildren(el("div", { class: "who" }, [el("div", { class: "name", text: "not authenticated" })]));
  }

  // Prime cached items (and thus unread badges) for every root query, mirroring
  // the TUI's startup load. Skip the first entry's root query: previewEntry(0)
  // below loads it, so priming it here would be a redundant double load.
  const firstRoot = state.entries.length ? state.entries[0].rootQueryId : null;
  const seen = new Set();
  for (const e of state.entries) {
    if (e.rootQueryId === firstRoot) continue;
    if (seen.has(e.rootQueryId)) continue;
    seen.add(e.rootQueryId);
    call("load_cached", { queryId: e.rootQueryId });
  }

  // The signed-in user is shown in the sidebar header; only the unauthenticated
  // case warrants a status message.
  if (!state.currentUser) setStatus("Not authenticated (set GH_TOKEN)");
  if (state.entries.length) {
    // Startup selection mirrors the GUI: sync only if the cache is stale, then
    // let the engine background-sync the remaining stale queries.
    previewEntry(0);
    const e0 = state.entries[0];
    if (!e0.isFilterStream) {
      call("sync_if_stale", { queryId: e0.rootQueryId, queryStr: e0.queryStr });
    }
    call("enqueue_stale", { skipQueryId: firstRoot });
  } else {
    setStatus("No saved queries. Add one with the TUI/GUI, then reopen.");
  }
}

main().catch((e) => setStatus(`init failed: ${e}`, true));
