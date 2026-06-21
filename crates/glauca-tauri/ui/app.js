// glauca-tauri front-end.
//
// Uses the global Tauri API (`withGlobalTauri: true` in tauri.conf.json), so no
// npm/@tauri-apps/api and no build step — the file is served as-is. Two channels:
//   * invoke('<command>', {...})  → engine (commands.rs); args are camelCase and
//     Tauri maps them to the snake_case Rust params.
//   * listen('app-message', ...)  ← engine; payload is the adjacently-tagged
//     AppMessage: { type: "ItemsLoaded", data: {...} }.
//
// This is a deliberately scoped baseline (browse / sync / read / act on items).
// Query & filter-stream editing dialogs and full filter-stream matching are not
// yet ported from the TUI/GUI — see README.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const state = {
  entries: [],            // normalized left-pane entries
  rawEntries: [],         // original LeftPaneEntry JSON ({type,data}) for unread_counts
  currentUser: null,
  itemsByQuery: new Map(), // rootQueryId -> ItemEntry[]
  pending: new Map(),      // rootQueryId -> held-back background items (not yet applied)
  visibleItems: [],        // current entry's items after stream + inline filtering
  unread: new Map(),       // unreadKey(isFilterStream, entryId) -> count
  selectedEntry: -1,
  selectedItemKey: null,
  filterText: "",
  comments: [],            // last-loaded comments (shown in the comments modal)
  commentsShowMinimized: false,
  commentsSortNewest: false,
  settings: { theme: "system", notifications_enabled: false, sync_interval_secs: 60 },
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

function setStatus(msg, isError = false) {
  $("status").textContent = msg;
  $("statusbar").classList.toggle("error", isError);
}

function itemKey(it) {
  return `${it.repo_owner}/${it.repo_name}#${it.number}`;
}

// How many items in `fresh` are new or changed vs `prev` (keyed by repo/number).
// Mirrors glauca_core::logic::count_changed — kept in JS as it is display-only.
function countChanged(prev, fresh) {
  const seen = new Map(prev.map((it) => [itemKey(it), it.updated_at]));
  return fresh.reduce((n, it) => {
    const k = itemKey(it);
    return !seen.has(k) || seen.get(k) !== it.updated_at ? n + 1 : n;
  }, 0);
}

// Show/hide the "N updated" banner based on held-back background items for the
// currently-selected query. Clicking it applies the pending items.
function updateBanner() {
  const banner = $("banner");
  const e = state.entries[state.selectedEntry];
  const fresh = e ? state.pending.get(e.rootQueryId) : null;
  if (!e || !fresh) {
    banner.hidden = true;
    return;
  }
  const n = countChanged(state.itemsByQuery.get(e.rootQueryId) || [], fresh);
  banner.textContent = `${n} updated in background — click to refresh`;
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
      lastViewedAt: d.last_viewed_at || null,
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
    lastViewedAt: d.last_viewed_at || null,
  };
}

function unreadKey(isFilterStream, entryId) {
  return `${isFilterStream ? 1 : 0}:${entryId}`;
}

// Recompute unread badges for every entry under `rootQueryId` by delegating to
// the engine's unread_counts command, which reuses glauca-core's
// compute_unread_counts — correct filter-stream scoping and the same
// "new-since-last-viewed AND unread" definition the TUI/GUI use. Driven by the
// front-end's in-memory items so it reflects reads immediately (no DB round-trip
// race after mark_item_read).
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

// Lightweight context menu at (x, y). `items` is [{label, onClick}] (a null entry
// renders a separator). Closes on selection or any outside click/Escape.
function showContextMenu(x, y, items) {
  document.querySelectorAll(".ctx-menu").forEach((m) => m.remove());
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
          menu.remove();
          it.onClick();
        },
      })
    );
  }
  const close = (ev) => {
    if (ev.type === "keydown" && ev.key !== "Escape") return;
    menu.remove();
    document.removeEventListener("mousedown", close, true);
    document.removeEventListener("keydown", close, true);
  };
  document.addEventListener("mousedown", close, true);
  document.addEventListener("keydown", close, true);
  document.body.appendChild(menu);
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
// itemsByQuery (read flags mutated locally stay consistent).
async function refreshVisible() {
  const e = state.entries[state.selectedEntry];
  const all = e ? state.itemsByQuery.get(e.rootQueryId) || [] : [];
  if (!e) {
    state.visibleItems = [];
    renderItemList();
    return;
  }
  try {
    const indices = await invoke("filter_items", {
      items: all,
      streamFilter: e.streamFilter,
      inlineFilter: state.filterText,
    });
    state.visibleItems = indices.map((i) => all[i]);
  } catch (err) {
    setStatus(`filter: ${err}`, true);
    state.visibleItems = all;
  }
  renderItemList();
}

function stateClass(s) {
  if (s === "open") return "state-open";
  if (s === "merged") return "state-merged";
  return "state-closed";
}

// CSS modifier for a GitHub review state (mirrors logic::ReviewState::from_state).
function reviewStateClass(state) {
  switch (state) {
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

// An avatar <img> (or an initial fallback when no avatar_url is known).
function avatarEl(user, cls = "avatar") {
  if (user && user.avatar_url) {
    const img = el("img", { class: cls });
    img.src = user.avatar_url;
    img.alt = user.login || "";
    img.title = user.login || "";
    return img;
  }
  const initial = (user && user.login ? user.login[0] : "?").toUpperCase();
  const span = el("span", { class: `${cls} avatar-fallback`, text: initial });
  if (user) span.title = user.login;
  return span;
}

// A small cluster of reviewer avatars, each ringed by its review state.
function reviewerCluster(it) {
  const overlays = reviewerOverlays(it);
  if (!overlays.length) return null;
  return el(
    "span",
    { class: "reviewers" },
    overlays.map((o) => avatarEl(o.user, `avatar sm ${reviewStateClass(o.state)}`))
  );
}

function renderItemList() {
  const list = $("item-list");
  list.replaceChildren();
  const e = state.entries[state.selectedEntry];
  $("items-title").textContent = e ? e.label : "Items";
  for (const it of state.visibleItems) {
    const key = itemKey(it);
    const meta = el("div", { class: "it-meta" }, [
      it.repo_private ? el("span", { class: "lock", text: "🔒" }) : null,
      el("span", { class: stateClass(it.state), text: it.state }),
      el("span", { text: `${it.repo_owner}/${it.repo_name} #${it.number}` }),
      it.author ? avatarEl(it.author, "avatar sm") : null,
      it.author ? el("span", { text: `@${it.author.login}` }) : null,
      it.is_new ? el("span", { class: "tag new", text: "NEW" }) : null,
      it.is_draft ? el("span", { class: "tag draft", text: "draft" }) : null,
      reviewerCluster(it),
    ]);
    const li = el(
      "li",
      {
        class: [it.read ? "read" : "", key === state.selectedItemKey ? "selected" : ""].filter(Boolean).join(" "),
        onclick: () => selectItem(it),
      },
      [el("span", { class: "it-title", text: it.title }), meta]
    );
    list.appendChild(li);
  }
}

function renderDetail(it) {
  const body = $("detail-body");
  body.classList.remove("empty");
  body.replaceChildren();

  body.appendChild(el("h2", { text: it.title }));
  const metaBits = [
    it.repo_private ? el("span", { class: "lock", text: "🔒" }) : null,
    el("span", { class: stateClass(it.state), text: it.state }),
    el("span", { text: `${it.repo_owner}/${it.repo_name} #${it.number}` }),
  ].filter(Boolean);
  if (it.author) metaBits.push(el("span", {}, [avatarEl(it.author, "avatar sm"), el("span", { text: ` @${it.author.login}` })]));
  if (it.review_decision) metaBits.push(el("span", { class: `decision ${reviewStateClass(it.review_decision)}`, text: it.review_decision }));
  if (it.milestone) metaBits.push(el("span", { text: `🎯 ${it.milestone}` }));
  body.appendChild(el("div", { class: "meta" }, metaBits.flatMap((b) => [b, document.createTextNode(" · ")]).slice(0, -1)));

  if (it.kind === "pull_request" && (it.base_ref || it.head_ref)) {
    body.appendChild(el("div", { class: "meta", text: `${it.head_ref || "?"} → ${it.base_ref || "?"}` }));
  }

  if (it.labels && it.labels.length) {
    body.appendChild(el("div", { class: "labels" }, it.labels.map((l) => el("span", { class: "tag", text: l }))));
  }

  // People: assignees and reviewers (with review state) as avatar chips.
  if (it.assignees && it.assignees.length) {
    body.appendChild(
      el("div", { class: "people" }, [
        el("span", { class: "people-label", text: "Assignees:" }),
        ...it.assignees.map((u) => el("span", { class: "chip" }, [avatarEl(u, "avatar sm"), el("span", { text: u.login })])),
      ])
    );
  }
  const overlays = reviewerOverlays(it);
  if (overlays.length) {
    body.appendChild(
      el("div", { class: "people" }, [
        el("span", { class: "people-label", text: "Reviewers:" }),
        ...overlays.map((o) =>
          el("span", { class: `chip ${reviewStateClass(o.state)}` }, [avatarEl(o.user, "avatar sm"), el("span", { text: o.user.login })])
        ),
      ])
    );
  }

  // Actions. The engine performs the external work (gh CLI / browser); the
  // front-end only sends the command.
  const entry = state.entries[state.selectedEntry];
  const actions = el("div", { class: "actions" });
  actions.appendChild(el("button", { text: "Open in browser", onclick: () => invoke("open_browser", { item: it }) }));
  actions.appendChild(el("button", { text: "Copy URL", onclick: () => copyText(it.url) }));
  actions.appendChild(
    el("button", {
      text: "Refresh",
      onclick: () =>
        invoke("refresh_item", {
          queryId: entry.rootQueryId,
          repoOwner: it.repo_owner,
          repoName: it.repo_name,
          number: it.number,
          highlightSince: entry.lastViewedAt,
        }),
    })
  );
  actions.appendChild(
    el("button", {
      text: "View comments",
      onclick: () => invoke("load_comments", { owner: it.repo_owner, repo: it.repo_name, number: it.number }),
    })
  );
  actions.appendChild(
    el("button", {
      text: "Comment",
      onclick: async () => {
        const text = await promptModal("Comment body:");
        if (text) invoke("comment", { url: it.url, kind: it.kind, body: text });
      },
    })
  );
  if (it.kind === "pull_request") {
    actions.appendChild(el("button", { text: "Approve", onclick: () => invoke("submit_review", { url: it.url, event: "approve", body: null }) }));
    actions.appendChild(
      el("button", {
        text: "Request changes",
        onclick: async () => {
          const text = await promptModal("Request changes — comment:");
          if (text) invoke("submit_review", { url: it.url, event: "request_changes", body: text });
        },
      })
    );
    actions.appendChild(
      el("button", {
        text: "Review comment",
        onclick: async () => {
          const text = await promptModal("Review comment:");
          if (text) invoke("submit_review", { url: it.url, event: "comment", body: text });
        },
      })
    );
    actions.appendChild(
      el("button", {
        text: "Merge",
        onclick: async () => {
          const s = ((await promptModal("Merge strategy (squash / merge / rebase):", "squash")) || "").trim();
          if (["squash", "merge", "rebase"].includes(s)) invoke("merge", { url: it.url, strategy: s });
        },
      })
    );
  }
  body.appendChild(actions);

  body.appendChild(el("div", { class: "body", text: it.body && it.body.length ? it.body : "(no description)" }));
}

// Open the comments overlay for the loaded comments (set by CommentsLoaded).
// Supports toggling minimized comments and oldest/newest sort.
function openCommentsModal() {
  document.querySelectorAll(".modal-overlay").forEach((m) => m.remove());
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
  const close = el("button", { text: "Close", onclick: () => overlay.remove() });
  const header = el("div", { class: "comments-header" }, [
    el("span", { class: "modal-title", text: `Comments (${state.comments.length})` }),
    minToggle,
    sortToggle,
    close,
  ]);

  render();
  overlay.appendChild(el("div", { class: "modal-box comments-box" }, [header, listBox]));
  overlay.addEventListener("keydown", (ev) => {
    if (ev.key === "Escape") overlay.remove();
  });
  document.body.appendChild(overlay);
}

// ── actions ──────────────────────────────────────────────────────────────────

function selectEntry(idx) {
  state.selectedEntry = idx;
  state.selectedItemKey = null;
  const e = state.entries[idx];
  // highlight_since is the entry's PERSISTED last-viewed baseline. Selecting does
  // NOT advance it (mirrors the TUI/GUI): items new since the last visit stay
  // highlighted, and the unread badge clears per-item as items are read.
  const since = e.lastViewedAt;

  invoke("load_cached", { queryId: e.rootQueryId, highlightSince: since });
  if (!e.isFilterStream) {
    invoke("sync", { queryId: e.rootQueryId, queryStr: e.queryStr, highlightSince: since });
  }

  renderSidebar();
  refreshVisible();
  updateBanner();
  $("detail-body").className = "empty";
  $("detail-body").textContent = "Select an item.";
}

function selectItem(it) {
  state.selectedItemKey = itemKey(it);
  if (!it.read) {
    const e = state.entries[state.selectedEntry];
    invoke("mark_item_read", { queryId: e.rootQueryId, repoOwner: it.repo_owner, repoName: it.repo_name, number: it.number });
    it.read = true;
    refreshUnread(e.rootQueryId); // recompute badges (also re-renders the sidebar)
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
  if (out) invoke("add_query", { name: out.name || null, query: out.query });
}

async function editQuery(e) {
  const out = await formModal("Edit query", [
    { key: "name", label: "Name (optional)", value: e.label },
    { key: "query", label: "GitHub search query", value: e.queryStr, required: true },
  ]);
  if (out) invoke("edit_query", { id: e.id, name: out.name || null, query: out.query });
}

async function newFilterStream(parent) {
  const out = await formModal(`New filter stream under "${parent.label}"`, [
    { key: "name", label: "Name", required: true },
    { key: "filter", label: "Filter (e.g. state:open label:bug)", required: true },
  ]);
  if (out) invoke("add_filter_stream", { parentId: parent.rootQueryId, kind: parent.kind, name: out.name, filter: out.filter });
}

async function editFilterStream(e) {
  const out = await formModal("Edit filter stream", [
    { key: "name", label: "Name", value: e.label, required: true },
    { key: "filter", label: "Filter", value: e.streamFilter, required: true },
  ]);
  if (out) invoke("edit_filter_stream", { id: e.id, name: out.name, filter: out.filter });
}

async function deleteEntry(e) {
  const what = e.isFilterStream ? "filter stream" : "query (and its filter streams)";
  if (!(await confirmModal(`Delete this ${what}: "${e.label}"?`))) return;
  if (e.isFilterStream) invoke("delete_filter_stream", { id: e.id });
  else invoke("delete_query", { queryId: e.id });
}

function moveEntry(idx, down) {
  const r = reorderArgs(idx, down);
  if (r) invoke(r.cmd, r.args);
}

function markAllRead(e) {
  invoke("mark_all_read", { queryId: e.rootQueryId, filter: e.isFilterStream ? e.streamFilter : null });
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
    items.push({
      label: "Full resync",
      onClick: () => invoke("full_resync", { queryId: e.rootQueryId, queryStr: e.queryStr, highlightSince: e.lastViewedAt }),
    });
  }
  showContextMenu(ev.clientX, ev.clientY, items);
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
      setStatus(`syncing #${d.query_id}…`);
      break;
    case "SyncDone":
      setStatus(`synced ${d.count} item(s)`);
      break;
    case "SyncError":
      setStatus(`sync error: ${d.error}`, true);
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
    // Remaining variants (EntryViewed, BgSync*) aren't surfaced here; ignore.
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

function openSettingsModal() {
  const s = state.settings;
  const overlay = el("div", { class: "modal-overlay" });
  const finish = () => overlay.remove();

  const themeSel = document.createElement("select");
  themeSel.className = "modal-input";
  for (const t of ["system", "light", "dark"]) {
    const opt = document.createElement("option");
    opt.value = t;
    opt.textContent = t;
    if (t === s.theme) opt.selected = true;
    themeSel.appendChild(opt);
  }
  // Live theme preview as the user changes the select.
  themeSel.addEventListener("change", () => applyTheme(themeSel.value));

  const notif = el("input");
  notif.type = "checkbox";
  notif.checked = !!s.notifications_enabled;

  const interval = el("input", { class: "modal-input" });
  interval.type = "number";
  interval.min = "10";
  interval.value = String(s.sync_interval_secs);

  const save = el("button", {
    text: "Save",
    onclick: async () => {
      const next = {
        theme: themeSel.value,
        notifications_enabled: notif.checked,
        sync_interval_secs: Math.max(10, parseInt(interval.value, 10) || 60),
      };
      try {
        await invoke("save_settings", {
          theme: next.theme,
          notificationsEnabled: next.notifications_enabled,
          syncIntervalSecs: next.sync_interval_secs,
        });
        state.settings = next;
        applyTheme(next.theme);
        setStatus("settings saved (sync interval applies on restart)");
      } catch (e) {
        setStatus(`settings: ${e}`, true);
      }
      finish();
    },
  });
  const cancel = el("button", {
    text: "Cancel",
    onclick: () => {
      applyTheme(s.theme); // revert live preview
      finish();
    },
  });

  overlay.appendChild(
    el("div", { class: "modal-box" }, [
      el("div", { class: "modal-title", text: "Settings" }),
      el("div", { class: "modal-label", text: "Theme" }),
      themeSel,
      el("label", { class: "modal-check" }, [notif, el("span", { text: " Desktop notifications" })]),
      el("div", { class: "modal-label", text: "Sync interval (seconds)" }),
      interval,
      el("div", { class: "modal-actions" }, [cancel, save]),
    ])
  );
  document.body.appendChild(overlay);
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

  $("new-query").addEventListener("click", newQuery);
  $("settings-btn").addEventListener("click", openSettingsModal);

  // Load + apply persisted settings (theme) before anything renders.
  try {
    state.settings = await invoke("get_settings");
  } catch {
    /* defaults */
  }
  applyTheme(state.settings.theme);
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

  // Prime cached items (and thus unread badges) for every root query, mirroring
  // the TUI's startup load. Skip the first entry's root query: selectEntry(0)
  // below loads it, so priming it here would be a redundant double load.
  const firstRoot = state.entries.length ? state.entries[0].rootQueryId : null;
  const seen = new Set();
  for (const e of state.entries) {
    if (e.rootQueryId === firstRoot) continue;
    if (seen.has(e.rootQueryId)) continue;
    seen.add(e.rootQueryId);
    invoke("load_cached", { queryId: e.rootQueryId, highlightSince: e.lastViewedAt });
  }

  setStatus(state.currentUser ? `Signed in as @${state.currentUser}` : "Not authenticated (set GH_TOKEN)");
  if (state.entries.length) selectEntry(0);
  else setStatus("No saved queries. Add one with the TUI/GUI, then reopen.");
}

main().catch((e) => setStatus(`init failed: ${e}`, true));
