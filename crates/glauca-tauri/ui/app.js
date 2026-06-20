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
  visibleItems: [],        // current entry's items after stream + inline filtering
  unread: new Map(),       // unreadKey(isFilterStream, entryId) -> count
  selectedEntry: -1,
  selectedItemKey: null,
  filterText: "",
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

// Minimal in-page text prompt. Replaces window.prompt(), which is not reliably
// implemented across Tauri/wry webviews (notably macOS WKWebView). Resolves to
// the entered string, or null if cancelled.
function promptModal(label, def = "") {
  return new Promise((resolve) => {
    const input = el("input", { class: "modal-input" });
    input.type = "text";
    input.value = def;
    const overlay = el("div", { class: "modal-overlay" });
    const finish = (val) => {
      overlay.remove();
      resolve(val);
    };
    const ok = el("button", { text: "OK", onclick: () => finish(input.value) });
    const cancel = el("button", { text: "Cancel", onclick: () => finish(null) });
    input.addEventListener("keydown", (ev) => {
      if (ev.key === "Enter") finish(input.value);
      else if (ev.key === "Escape") finish(null);
    });
    overlay.appendChild(
      el("div", { class: "modal-box" }, [
        el("div", { class: "modal-label", text: label }),
        input,
        el("div", { class: "modal-actions" }, [cancel, ok]),
      ])
    );
    document.body.appendChild(overlay);
    input.focus();
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
    list.appendChild(el("li", { class: cls, onclick: () => selectEntry(idx) }, children));
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

function renderItemList() {
  const list = $("item-list");
  list.replaceChildren();
  const e = state.entries[state.selectedEntry];
  $("items-title").textContent = e ? e.label : "Items";
  for (const it of state.visibleItems) {
    const key = itemKey(it);
    const meta = el("div", { class: "it-meta" }, [
      el("span", { class: stateClass(it.state), text: it.state }),
      el("span", { text: `${it.repo_owner}/${it.repo_name} #${it.number}` }),
      it.author ? el("span", { text: `@${it.author.login}` }) : null,
      it.is_new ? el("span", { class: "tag new", text: "NEW" }) : null,
      it.is_draft ? el("span", { class: "tag draft", text: "draft" }) : null,
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
    el("span", { class: stateClass(it.state), text: it.state }),
    el("span", { text: `${it.repo_owner}/${it.repo_name} #${it.number}` }),
  ];
  if (it.author) metaBits.push(el("span", { text: `by @${it.author.login}` }));
  if (it.review_decision) metaBits.push(el("span", { text: it.review_decision }));
  body.appendChild(el("div", { class: "meta" }, metaBits.flatMap((b) => [b, document.createTextNode(" · ")]).slice(0, -1)));

  if (it.labels && it.labels.length) {
    body.appendChild(el("div", { class: "labels" }, it.labels.map((l) => el("span", { class: "tag", text: l }))));
  }

  // Actions. The engine performs the external work (gh CLI / browser); the
  // front-end only sends the command.
  const actions = el("div", { class: "actions" });
  actions.appendChild(el("button", { text: "Open in browser", onclick: () => invoke("open_browser", { item: it }) }));
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

  // Placeholder the comments load fills in.
  body.appendChild(el("div", { class: "comments", text: "" }));
}

function renderComments(comments) {
  const holder = document.querySelector("#detail-body .comments");
  if (!holder) return;
  holder.replaceChildren();
  if (!comments.length) {
    holder.appendChild(el("div", { class: "chead", text: "No comments." }));
    return;
  }
  holder.appendChild(el("div", { class: "pane-title", text: `Comments (${comments.length})` }));
  for (const c of comments) {
    const head = c.is_minimized
      ? `${c.author} · ${c.created_at} · minimized${c.minimized_reason ? ` (${c.minimized_reason})` : ""}`
      : `${c.author} · ${c.created_at}`;
    holder.appendChild(
      el("div", { class: "comment" }, [el("div", { class: "chead", text: head }), el("div", { class: "cbody", text: c.body })])
    );
  }
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

// ── engine messages ────────────────────────────────────────────────────────--

function handleMessage(msg) {
  const d = msg.data;
  switch (msg.type) {
    case "ItemsLoaded": {
      const e = state.entries[state.selectedEntry];
      const isCurrent = e && e.rootQueryId === d.query_id;
      if (d.background && isCurrent) {
        // Engine contract: defer background-sync results for the query the user
        // is viewing so the list doesn't reshuffle under them. Hold them back;
        // reselecting (or selecting another entry and returning) reloads fresh.
        setStatus("updated in background — reselect to refresh");
        break;
      }
      state.itemsByQuery.set(d.query_id, d.items);
      if (isCurrent) refreshVisible();
      refreshUnread(d.query_id);
      break;
    }
    case "CommentsLoaded":
      renderComments(d);
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
    // Other variants (Query/FilterStream add/edit/delete/swap, EntryViewed,
    // BgSync*) aren't surfaced by this baseline UI; ignore them quietly.
    default:
      break;
  }
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
