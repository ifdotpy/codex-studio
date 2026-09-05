const $ = (s) => document.querySelector(s);
const $$ = (s) => [...document.querySelectorAll(s)];
const escape = (value) =>
  String(value ?? "").replace(
    /[&<>"']/g,
    (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[
        c
      ],
  );
const number = (n) =>
  new Intl.NumberFormat("en", {
    notation: "compact",
    maximumFractionDigits: 1,
  }).format(n || 0);
const fullNumber = (n) => new Intl.NumberFormat("en").format(n || 0);
const active = new Set(["starting", "running", "waiting", "capacity-retry"]);
const attention = new Set([
  "blocked",
  "failed",
  "abandoned",
  "interrupted",
  "paused",
  "stalled",
  "capacity-retry",
]);
const tone = (t) =>
  t.status === "completed"
    ? "done"
    : ["abandoned", "failed"].includes(t.status)
      ? "offline"
      : attention.has(t.status)
        ? "attention"
        : "active";
const label = (t) =>
  ({
    "capacity-retry": "Capacity retry",
    abandoned: "Launcher offline",
    completed: "Complete",
    blocked: "Blocked",
    running: "Working",
    waiting: "Between turns",
    starting: "Starting",
    failed: "Failed",
    paused: "Paused",
    interrupted: "Interrupted",
  })[t.status] || t.status;
const age = (value) => {
  if (!value) return "No events";
  const s = Math.max(0, (Date.now() - Date.parse(value)) / 1000);
  return s < 60
    ? "just now"
    : s < 3600
      ? `${Math.floor(s / 60)}m ago`
      : s < 86400
        ? `${Math.floor(s / 3600)}h ago`
        : `${Math.floor(s / 86400)}d ago`;
};
const stamp = (value) =>
  value
    ? new Date(
        typeof value === "number" ? value * 1000 : value,
      ).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
    : "";
const state = {
  threads: [],
  chats: [],
  edges: [],
  selectedEdge: null,
  connectFrom: null,
  mode: "select",
  space: false,
  board: { claims: {}, notes: [], queue: {} },
  token: "",
  filter: "all",
  query: "",
  selected: new Set(),
  opened: null,
  tab: "activity",
  positions: {},
  view: { x: 45, y: 45, z: 0.85 },
  storageKey: null,
  drag: null,
  detailKey: "",
  pendingSend: null,
  pendingGroup: null,
  sending: false,
  creatingGroup: false,
};
let toastTimer, refreshTimer, detailTimer, saveTimer;

function toast(text) {
  $("#toast").textContent = text;
  $("#toast").hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => ($("#toast").hidden = true), 4500);
}
async function api(path, body) {
  const r = await fetch(
    path,
    body
      ? {
          method: "POST",
          headers: {
            "Content-Type": "application/json",
            "X-Canvas-Token": state.token,
          },
          body: JSON.stringify(body),
        }
      : {},
  );
  const value = await r.json();
  if (!r.ok) throw new Error(value.error || `Request failed (${r.status})`);
  return value;
}
function save() {
  clearTimeout(saveTimer);
  saveTimer = setTimeout(() => {
    try {
      localStorage.setItem(
        state.storageKey,
        JSON.stringify({ positions: state.positions, view: state.view }),
      );
    } catch {
      toast("The browser cannot save this layout.");
    }
  }, 200);
}
function loadLayout(key) {
  state.storageKey = `codex-canvas-graph:${key}`;
  try {
    const saved = JSON.parse(localStorage.getItem(state.storageKey));
    if (saved) {
      state.positions = Object.fromEntries(
        Object.entries(saved.positions || {}).filter(
          ([, p]) => Number.isFinite(p.x) && Number.isFinite(p.y),
        ),
      );
      if (
        saved.view &&
        ["x", "y", "z"].every((k) => Number.isFinite(saved.view[k]))
      )
        state.view = {
          ...saved.view,
          z: Math.max(0.2, Math.min(1.6, saved.view.z)),
        };
    }
  } catch {
    /* Ignore an invalid device-local preference. */
  }
}
const NODE_W = 208,
  NODE_H = 76;
function nodes() {
  return [...state.threads, ...state.chats];
}
function visible() {
  return nodes().filter(
    (t) =>
      (t.kind === "chat" ||
        state.filter === "all" ||
        (state.filter === "active" ? active : attention).has(t.status)) &&
      [t.name, t.wave, t.branch, t.cwd, t.status, t.model].some((v) =>
        String(v || "")
          .toLowerCase()
          .includes(state.query),
      ),
  );
}
function arrange(reset = false) {
  const shown = visible(),
    ids = new Set(shown.map((n) => n.id)),
    ordered = [],
    seen = new Set();
  const add = (n) => {
    if (seen.has(n.id)) return;
    seen.add(n.id);
    ordered.push(n);
    for (const child of shown.filter((t) => t.parentId === n.id)) add(child);
  };
  const rank = (n) =>
    n.role === "orchestrator"
      ? 0
      : active.has(n.status)
        ? 1
        : attention.has(n.status)
          ? 2
          : 3;
  shown
    .filter((n) => n.kind === "agent" && !ids.has(n.parentId))
    .sort((a, b) => rank(a) - rank(b))
    .forEach(add);
  shown.forEach(add);
  const columns = Math.max(
    2,
    Math.floor(($("#viewport").clientWidth - 80) / 255),
  );
  ordered.forEach((n, i) => {
    if (reset || !state.positions[n.id])
      state.positions[n.id] = {
        x: 40 + (i % columns) * 255,
        y: 40 + Math.floor(i / columns) * 108,
      };
  });
  save();
}
function bounds(items = visible()) {
  const ps = items.map((t) => state.positions[t.id]).filter(Boolean);
  if (!ps.length) return { x: 0, y: 0, w: 700, h: 400 };
  const x = Math.min(...ps.map((p) => p.x)) - 20,
    y = Math.min(...ps.map((p) => p.y)) - 20;
  return {
    x,
    y,
    w: Math.max(...ps.map((p) => p.x + NODE_W)) - x + 20,
    h: Math.max(...ps.map((p) => p.y + NODE_H)) - y + 20,
  };
}
function fit() {
  const b = bounds(),
    v = $("#viewport");
  const z = Math.max(
    0.2,
    Math.min(1, (v.clientWidth - 95) / b.w, (v.clientHeight - 100) / b.h),
  );
  state.view = {
    z,
    x: (v.clientWidth - b.w * z) / 2 - b.x * z,
    y: (v.clientHeight - b.h * z) / 2 - b.y * z - 12,
  };
  transform();
  save();
}
function transform() {
  const v = state.view;
  $("#world").style.transform = `translate(${v.x}px,${v.y}px) scale(${v.z})`;
  $("#zoom").textContent = `${Math.round(v.z * 100)}%`;
  $("#viewport").style.backgroundSize = `${24 * v.z}px ${24 * v.z}px`;
  $("#viewport").style.backgroundPosition = `${v.x}px ${v.y}px`;
  drawMinimap();
}
function zoom(
  mult,
  cx = $("#viewport").clientWidth / 2,
  cy = $("#viewport").clientHeight / 2,
) {
  const v = state.view,
    z = Math.max(0.2, Math.min(1.6, v.z * mult)),
    k = z / v.z;
  state.view = { z, x: cx - (cx - v.x) * k, y: cy - (cy - v.y) * k };
  transform();
  save();
}
function drawMinimap() {
  const canvas = $("#minimap"),
    ctx = canvas.getContext("2d"),
    b = bounds();
  ctx.clearRect(0, 0, 190, 110);
  const z = Math.min(166 / b.w, 86 / b.h),
    ox = 12 - b.x * z,
    oy = 12 - b.y * z;
  for (const t of visible()) {
    const p = state.positions[t.id];
    if (!p) continue;
    ctx.fillStyle = state.selected.has(t.id)
      ? "#b4e894"
      : tone(t) === "offline"
        ? "#775656"
        : tone(t) === "attention"
          ? "#796849"
          : "#505862";
    ctx.fillRect(ox + p.x * z, oy + p.y * z, NODE_W * z, NODE_H * z);
  }
  ctx.strokeStyle = "#b4e89488";
  ctx.lineWidth = 1;
  const v = state.view;
  ctx.strokeRect(
    ox - (v.x / v.z) * z,
    oy - (v.y / v.z) * z,
    ($("#viewport").clientWidth / v.z) * z,
    ($("#viewport").clientHeight / v.z) * z,
  );
}
function renderEdges() {
  const svg = $("#connections"),
    ids = new Set(visible().map((n) => n.id));
  svg.replaceChildren();
  const defs = document.createElementNS("http://www.w3.org/2000/svg", "defs");
  defs.innerHTML =
    '<marker id="spawn-arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="5" markerHeight="5" orient="auto-start-reverse"><path d="M 0 0 L 10 5 L 0 10 Z" fill="#8cad84" /></marker>';
  svg.append(defs);
  for (const edge of state.edges) {
    if (!ids.has(edge.source) || !ids.has(edge.target)) continue;
    const a = state.positions[edge.source],
      b = state.positions[edge.target];
    if (!a || !b) continue;
    const start = { x: a.x + NODE_W, y: a.y + NODE_H / 2 },
      end = { x: b.x, y: b.y + NODE_H / 2 };
    const bend = Math.max(60, Math.abs(end.x - start.x) * 0.45);
    const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
    path.setAttribute(
      "d",
      `M ${start.x} ${start.y} C ${start.x + bend} ${start.y}, ${end.x - bend} ${end.y}, ${end.x} ${end.y}`,
    );
    path.setAttribute(
      "class",
      `connection ${edge.kind}${state.selectedEdge === edge.id ? " selected" : ""}`,
    );
    path.dataset.edge = edge.id;
    if (edge.kind === "spawn")
      path.setAttribute("marker-end", "url(#spawn-arrow)");
    path.setAttribute("tabindex", "0");
    path.setAttribute("role", "button");
    const source = nodes().find((n) => n.id === edge.source),
      target = nodes().find((n) => n.id === edge.target);
    path.setAttribute(
      "aria-label",
      `${source?.name} ${edge.kind === "spawn" ? "created" : "participates in"} ${target?.name}`,
    );
    svg.append(path);
  }
}
function card(t) {
  const chat = t.kind === "chat",
    children = state.edges.some((e) => e.kind === "spawn" && e.source === t.id);
  const role = chat
    ? "Chat"
    : children || t.role === "orchestrator"
      ? "Orchestrator"
      : t.role || "Agent";
  const status = chat
    ? `${t.members.length} members · ${t.messageCount} messages`
    : label(t);
  return `<button class="node-port" data-port="${escape(t.id)}" aria-label="Connect ${escape(t.name)}" title="Connect to a chat"></button>
    <div class="compact-heading"><span class="node-symbol">${chat ? "▤" : children || t.role === "orchestrator" ? "◈" : "●"}</span><h3>${escape(t.name)}</h3><button class="node-open" data-open="${escape(t.id)}" aria-label="Open ${escape(t.name)}">↗</button></div>
    <div class="compact-meta"><span>${escape(role)}</span><span>${escape(status)}</span></div>
    <div class="card-tail">${escape(chat ? t.tail || "No messages yet" : t.error || t.tail || "No message yet")}</div>`;
}
function render() {
  if (state.drag) return;
  arrange();
  const shown = visible(),
    ids = new Set(shown.map((t) => t.id));
  for (const el of $$("#cards > *")) if (!ids.has(el.dataset.id)) el.remove();
  for (const t of shown) {
    let el = document.getElementById(`agent-${t.id}`);
    if (!el) {
      el = document.createElement("article");
      el.id = `agent-${t.id}`;
      el.dataset.id = t.id;
      el.tabIndex = 0;
      el.setAttribute("role", "button");
      $("#cards").append(el);
    }
    el.className = `agent-card${t.kind === "chat" ? " chat-node" : ""}${state.selected.has(t.id) ? " chosen" : ""}${state.opened?.id === t.id ? " open" : ""}${state.connectFrom === t.id ? " link-source" : ""}`;
    el.dataset.tone = t.kind === "chat" ? "chat" : tone(t);
    el.setAttribute(
      "aria-label",
      `${t.name}, ${t.kind === "chat" ? "Chat" : label(t)}. Select node. Double-click to open.`,
    );
    el.setAttribute("aria-pressed", String(state.selected.has(t.id)));
    const html = card(t);
    if (el.innerHTML !== html) el.innerHTML = html;
    const p = state.positions[t.id];
    el.style.left = `${p.x}px`;
    el.style.top = `${p.y}px`;
  }
  renderEdges();
  transform();
  $("#empty").hidden = shown.length > 0;
  $("#empty h2").textContent = nodes().length
    ? "No matching nodes"
    : "Start your board";
  $("#empty p").textContent = nodes().length
    ? "Change the search or filter."
    : "Create a chat or start agents with codex-agents.";
  $("#clear-filter").hidden = !nodes().length;
  $("#all-count").textContent = state.threads.length;
  $("#active-count").textContent = state.threads.filter((t) =>
    active.has(t.status),
  ).length;
  $("#attention-count").textContent = state.threads.filter((t) =>
    attention.has(t.status),
  ).length;
  $("#health").innerHTML =
    `<span><strong>${state.threads.length}</strong> agents</span><span><strong>${state.chats.length}</strong> chats</span>`;
  $("#total-tokens").textContent =
    `${number(state.threads.reduce((n, t) => n + (t.tokensUsed || 0), 0))} goal tokens`;
  $("#caption").textContent =
    `${shown.length} NODES · ${state.edges.length} CONNECTIONS`;
  $("#selection-bar").hidden = !state.selected.size && !state.selectedEdge;
  $("#selection-count").textContent = state.selectedEdge
    ? "Connection selected"
    : `${state.selected.size} selected`;
  $("#chat-selected").disabled = !state.selected.size;
  $("#disconnect").hidden = !state.edges.some(
    (e) => e.id === state.selectedEdge && e.kind === "chat",
  );
  $("#link-mode").classList.toggle("selected", state.mode === "connect");
  $("#pan-mode").classList.toggle("selected", state.mode === "pan");
  $("#select-mode").classList.toggle("selected", state.mode === "select");
}
function openNode(id) {
  const n = nodes().find((n) => n.id === id);
  if (n) openDetail(n.kind === "chat" ? "chat" : "agent", id);
}
async function connectNode(id) {
  if (!state.connectFrom) {
    state.connectFrom = id;
    render();
    toast("Select an agent or a chat to complete the connection.");
    return;
  }
  const a = nodes().find((n) => n.id === state.connectFrom),
    b = nodes().find((n) => n.id === id);
  if (!a || !b || a.kind === b.kind) {
    state.connectFrom = null;
    render();
    toast("Connect one agent to one chat.");
    return;
  }
  const source = a.kind === "chat" ? b.id : a.id,
    target = a.kind === "chat" ? a.id : b.id;
  try {
    await api("/api/connections", { source, target, connected: true });
    state.connectFrom = null;
    await refresh();
    toast("Agent connected to the chat.");
  } catch (error) {
    toast(error.message);
  }
}
async function refresh() {
  clearTimeout(refreshTimer);
  try {
    const result = await api("/api/state");
    const first = !state.storageKey;
    if (first) loadLayout(result.stateDir);
    Object.assign(state, {
      threads: result.threads,
      chats: result.chats,
      edges: result.edges,
      board: result.board,
      boardError: result.boardError,
      token: result.token,
    });
    state.selected = new Set(
      [...state.selected].filter((id) => nodes().some((t) => t.id === id)),
    );
    $("#error").hidden = true;
    $("#connection").textContent = `Live · Updated ${stamp(result.at)}`;
    $(".connection-dot").style.background = "var(--green)";
    render();
    if (state.opened?.kind === "resources") renderResources();
    if (state.opened?.kind === "chats") renderChats();
  } catch (error) {
    $("#error").hidden = false;
    $("#error").textContent =
      `Connection interrupted: ${error.message}. The canvas shows the last received state.`;
    $("#connection").textContent = "Disconnected";
    $(".connection-dot").style.background = "var(--red)";
  } finally {
    refreshTimer = setTimeout(refresh, 1800);
  }
}

function closeDetail() {
  state.opened = null;
  $("#inspector").hidden = true;
  clearTimeout(detailTimer);
  state.detailKey = "";
  render();
}
function openDetail(kind, id) {
  state.opened = { kind, id };
  state.tab = kind === "chat" ? "chat" : "activity";
  state.detailKey = "";
  state.pendingSend = null;
  $("#message").value = "";
  $("#inspector").hidden = false;
  render();
  loadDetail();
}
function setTab(tab) {
  state.tab = tab;
  state.detailKey = "";
  loadDetail();
}
function meta() {
  const o = state.opened;
  if (!o || ["resources", "chats"].includes(o.kind)) return;
  const t = state.threads.find((t) => t.id === o.id),
    g = state.chats.find((g) => g.id === o.id),
    isGroup = o.kind === "chat";
  $("#detail-label").textContent = isGroup
    ? "SHARED CHAT"
    : "AGENT / " + (t?.role || "worker").toUpperCase();
  $("#detail-title").textContent = isGroup
    ? g?.name || "Chat unavailable"
    : t?.name || "Agent unavailable";
  $("#detail-meta").innerHTML = isGroup
    ? `<span class="pill">${g?.members.length || 0} members</span><span>Persistent chat · local</span>`
    : `<span class="pill ${t && active.has(t.status) ? "active" : ""}">${escape(t ? label(t) : "Unavailable")}</span><span>${escape(t?.model || t?.requestedModel || "")}</span>`;
  $$("#detail-tabs button").forEach((b) => {
    b.hidden = isGroup && b.dataset.tab === "activity";
    b.classList.toggle("selected", b.dataset.tab === state.tab);
  });
  $("#detail-tabs").hidden = false;
  $("#stream-controls").hidden = state.tab !== "activity";
  $("#composer").hidden = state.tab === "details";
  $("#composer-label").textContent = isGroup
    ? "Message connected agents"
    : "Message this agent";
  const canSend = isGroup ? !!g : !!t?.canSend;
  $("#composer button").disabled = !canSend || state.sending;
  $("#message").disabled = !canSend || state.sending;
  $("#message").placeholder = canSend
    ? "Send an instruction…"
    : "No live mailbox for this agent";
  $("#send-note").textContent = isGroup
    ? "Each member gets your message. Offline members show a failure."
    : "Queued means mailbox receipt, not agent completion.";
}
function entry(item) {
  const text =
    escape(item.text) +
    (item.truncated ? "\n[Text clipped at 20,000 characters]" : "");
  return `<article class="entry ${escape(item.role)}"><div class="entry-header"><span>${item.role === "tool" ? "⌘" : item.role === "output" ? "↳" : "●"}</span>${escape(item.title)}<time>${stamp(item.at)}</time></div>${item.role === "output" ? `<details><summary>Read tool result (${fullNumber(item.text.length)} characters)</summary><div class="entry-text">${text}</div></details>` : `<div class="entry-text">${text}</div>`}</article>`;
}
function setBody(html, key) {
  if (key === state.detailKey) return;
  state.detailKey = key;
  const el = $("#detail-body"),
    scroll = el.scrollTop;
  const openIndexes = [...el.querySelectorAll("details")]
    .map((d, i) => (d.open ? i : -1))
    .filter((i) => i >= 0);
  el.innerHTML = html;
  for (const i of openIndexes) {
    const d = el.querySelectorAll("details")[i];
    if (d) d.open = true;
  }
  if ($("#follow").checked && state.tab !== "details")
    el.scrollTop = el.scrollHeight;
  else el.scrollTop = scroll;
}
async function loadDetail() {
  clearTimeout(detailTimer);
  const opened = state.opened;
  if (!opened || ["resources", "chats"].includes(opened.kind)) return;
  const tab = state.tab;
  meta();
  try {
    if (tab === "details") {
      const g = state.chats.find((g) => g.id === opened.id),
        t = state.threads.find((t) => t.id === opened.id);
      if (opened.kind === "chat") {
        setBody(
          `<p class="notice">Connections define chat membership. An agent can join several chats. Replacement runs do not inherit connections.</p>${(
            g?.members || []
          )
            .map((id) => {
              const m = state.threads.find((t) => t.id === id);
              return `<button class="member-button" data-member="${id}">${escape(m?.name || "Agent from an older run")}<small>${escape(m ? label(m) : "Unavailable")}</small></button>`;
            })
            .join("")}`,
          JSON.stringify(g) +
            JSON.stringify(state.threads.map((t) => t.status)),
        );
      } else {
        const fields = {
          Source: t?.source,
          "Reported status at": t?.reportedAt
            ? new Date(t.reportedAt * 1000).toISOString()
            : undefined,
          Parent: state.threads.find((n) => n.id === t?.parentId)?.name,
          Wave: t?.wave,
          Run: t?.runId,
          Thread: t?.threadId,
          Branch: t?.branch,
          Worktree: t?.cwd,
          Model: t?.model || t?.requestedModel,
          Effort: t?.effort,
          Goal: t?.goalStatus,
          "Goal tokens": fullNumber(t?.tokensUsed),
          Events: fullNumber(t?.events),
          "Last event": t?.lastEvent,
          Error: t?.error || "None",
        };
        setBody(
          `<dl class="detail-grid">${Object.entries(fields)
            .map(([k, v]) => `<dt>${k}</dt><dd>${escape(v || "Unknown")}</dd>`)
            .join("")}</dl>`,
          JSON.stringify(fields),
        );
      }
    } else if (tab === "activity") {
      const result = await api(`/api/transcript?id=${opened.id}`);
      if (state.opened !== opened || state.tab !== tab) return;
      const items = result.items.filter(
        (i) => $("#show-tools").checked || !["tool", "output"].includes(i.role),
      );
      const html =
        (result.truncated
          ? '<p class="notice">Recent transcript only. At most 2 MiB and 120 entries. The original local transcript stays complete.</p>'
          : "") +
        (result.unavailable
          ? `<p class="notice">${escape(result.unavailable)}</p><div class="entry-text">${escape(result.tail)}</div>`
          : "") +
        items.map(entry).join("");
      setBody(
        html || '<p class="notice">No visible messages yet.</p>',
        JSON.stringify(items) + result.unavailable + result.truncated,
      );
    } else {
      const messages = await api(`/api/messages?room=${opened.id}`);
      if (state.opened !== opened || state.tab !== tab) return;
      const g = state.chats.find((g) => g.id === opened.id);
      const html =
        (g
          ? '<p class="notice">Your messages go to connected agents. Agents can read and post here with codex-chat. Peer posts do not start turns automatically.</p>'
          : '<p class="notice">Instructions sent from this canvas. Read agent replies in Activity.</p>') +
        messages
          .map((m) => {
            const name =
              m.author === "user"
                ? "You"
                : state.threads.find((t) => t.id === m.author)?.name ||
                  "Agent from an older run";
            return (
              entry({
                role: m.author === "user" ? "user" : "assistant",
                title: name,
                text: m.text,
                at: m.at,
              }) +
              Object.entries(m.deliveries)
                .map(
                  ([id, status]) =>
                    `<div class="delivery ${status.startsWith("failed") ? "failed" : ""}">${escape(state.threads.find((t) => t.id === id)?.name || id)}: ${escape(status === "queued" ? "Queued in agent mailbox" : status === "pending" ? "Dispatch result unknown. Inspect the mailbox before resending." : status)}</div>`,
                )
                .join("")
            );
          })
          .join("");
      setBody(
        html +
          (messages.length
            ? ""
            : '<p class="notice">No messages yet. Send a task or a question to start the chat.</p>'),
        JSON.stringify(messages),
      );
    }
  } catch (error) {
    if (state.opened === opened)
      setBody(`<p class="notice">${escape(error.message)}</p>`, error.message);
  } finally {
    if (state.opened === opened) detailTimer = setTimeout(loadDetail, 1600);
  }
}
function renderResources() {
  if (state.opened?.kind !== "resources") return;
  $("#detail-label").textContent = "SHARED MACHINE";
  $("#detail-title").textContent = "Resources";
  $("#detail-meta").innerHTML =
    "<span>Claims and wait queues from codex-board</span>";
  $("#detail-tabs").hidden = true;
  $("#stream-controls").hidden = true;
  $("#composer").hidden = true;
  const claims = Object.entries(state.board.claims || {}),
    notes = state.board.notes || [],
    queues = Object.entries(state.board.queue || {});
  const html =
    (state.boardError
      ? `<p class="notice">${escape(state.boardError)}</p>`
      : "") +
    (claims.length
      ? claims
          .map(
            ([name, c]) =>
              `<div class="resource"><h3>${escape(name)} <span class="pill">${c.stale ? "STALE OWNER" : "CLAIMED"}</span></h3><p>${escape(c.worker)}<br>${escape(c.note || "")}</p></div>`,
          )
          .join("")
      : '<p class="notice">No resource claims.</p>') +
    queues
      .filter(([, q]) => q.length)
      .map(
        ([name, q]) =>
          `<div class="resource"><h3>${escape(name)} · queue</h3>${q.map((w, i) => `<p>${i + 1}. ${escape(w.worker)}</p>`).join("")}</div>`,
      )
      .join("") +
    (notes.length ? '<div class="notice">RECENT BOARD NOTES</div>' : "") +
    notes
      .slice(-15)
      .reverse()
      .map((n) =>
        entry({ role: "assistant", title: n.worker, text: n.text, at: n.at }),
      )
      .join("");
  setBody(html, html);
}

$("#viewport").addEventListener("pointerdown", (e) => {
  const port = e.target.closest("[data-port]");
  if (port) {
    e.preventDefault();
    state.portDrag = { id: port.dataset.port, x: e.clientX, y: e.clientY };
    return;
  }
  if (
    e.target.closest(
      "button,#minimap,.canvas-controls,#selection-bar,[data-edge]",
    )
  )
    return;
  if (e.button !== 0 && e.button !== 1) return;
  const node = e.target.closest(".agent-card");
  if (state.mode === "connect" && node) {
    connectNode(node.dataset.id);
    return;
  }
  const pan = state.mode === "pan" || state.space || e.button === 1;
  const id = pan ? null : node?.dataset.id;
  state.drag = {
    pointer: e.pointerId,
    startX: e.clientX,
    startY: e.clientY,
    id,
    pan,
    shift: e.shiftKey,
    moved: false,
    wasSelected: id && state.selected.has(id),
    view: { ...state.view },
    selected: new Set(state.selected),
    positions: {},
  };
  if (id) {
    if (!state.selected.has(id) && !e.shiftKey) state.selected.clear();
    state.selected.add(id);
    for (const key of state.selected)
      state.drag.positions[key] = { ...state.positions[key] };
  }
  $("#viewport").setPointerCapture(e.pointerId);
});
$("#viewport").addEventListener("pointermove", (e) => {
  const d = state.drag;
  if (!d || d.pointer !== e.pointerId) return;
  const dx = e.clientX - d.startX,
    dy = e.clientY - d.startY;
  if (Math.hypot(dx, dy) > 4) d.moved = true;
  if (!d.moved) return;
  if (d.id) {
    for (const [id, start] of Object.entries(d.positions)) {
      const p = {
        x: start.x + dx / state.view.z,
        y: start.y + dy / state.view.z,
      };
      state.positions[id] = p;
      const el = document.getElementById(`agent-${id}`);
      if (el) {
        el.style.left = `${p.x}px`;
        el.style.top = `${p.y}px`;
        el.classList.add("chosen");
      }
    }
    renderEdges();
    drawMinimap();
  } else if (d.pan) {
    state.view.x = d.view.x + dx;
    state.view.y = d.view.y + dy;
    transform();
  } else {
    const rect = $("#viewport").getBoundingClientRect(),
      x = Math.min(d.startX, e.clientX) - rect.left,
      y = Math.min(d.startY, e.clientY) - rect.top,
      w = Math.abs(dx),
      h = Math.abs(dy);
    const box = $("#marquee");
    box.hidden = false;
    Object.assign(box.style, {
      left: `${x}px`,
      top: `${y}px`,
      width: `${w}px`,
      height: `${h}px`,
    });
    const v = state.view;
    state.selected = d.shift ? new Set(d.selected) : new Set();
    for (const n of visible()) {
      const p = state.positions[n.id],
        nx = p.x * v.z + v.x,
        ny = p.y * v.z + v.y;
      if (
        nx < x + w &&
        nx + NODE_W * v.z > x &&
        ny < y + h &&
        ny + NODE_H * v.z > y
      )
        state.selected.add(n.id);
    }
    for (const el of $$(".agent-card"))
      el.classList.toggle("chosen", state.selected.has(el.dataset.id));
  }
});
function finishDrag(e, cancel = false) {
  const d = state.drag;
  if (!d || d.pointer !== e.pointerId) return;
  state.drag = null;
  $("#marquee").hidden = true;
  if (cancel) {
    state.selected = d.selected;
    for (const [id, p] of Object.entries(d.positions)) state.positions[id] = p;
    state.view = d.view;
  } else if (!d.moved) {
    if (d.id && d.shift && d.wasSelected) state.selected.delete(d.id);
    else if (!d.id && !d.pan && !d.shift) state.selected.clear();
  }
  state.selectedEdge = null;
  save();
  render();
}
$("#viewport").addEventListener("pointerup", (e) => finishDrag(e));
$("#viewport").addEventListener("pointercancel", (e) => {
  state.portDrag = null;
  finishDrag(e, true);
});
document.addEventListener("pointerup", (e) => {
  const d = state.portDrag;
  if (!d) return;
  state.portDrag = null;
  const el = document
    .elementFromPoint?.(e.clientX, e.clientY)
    ?.closest(".agent-card");
  if (el && el.dataset.id !== d.id) {
    state.connectFrom = d.id;
    connectNode(el.dataset.id);
  } else connectNode(d.id);
});
$("#cards").addEventListener("click", (e) => {
  const button = e.target.closest("[data-open]");
  if (button) openNode(button.dataset.open);
  const port = e.target.closest("[data-port]");
  if (port && e.detail === 0) connectNode(port.dataset.port);
});
$("#cards").addEventListener("dblclick", (e) => {
  const node = e.target.closest(".agent-card");
  if (node && !e.target.closest("button")) openNode(node.dataset.id);
});
$("#connections").addEventListener("click", (e) => {
  const edge = e.target.closest("[data-edge]");
  if (edge) {
    state.selectedEdge = edge.dataset.edge;
    state.selected.clear();
    render();
  }
});
$("#connections").addEventListener("keydown", (e) => {
  if (e.key === "Enter" || e.key === " ") {
    e.preventDefault();
    state.selectedEdge = e.target.dataset.edge;
    state.selected.clear();
    render();
  }
});
$("#viewport").addEventListener(
  "wheel",
  (e) => {
    e.preventDefault();
    const r = $("#viewport").getBoundingClientRect();
    if (e.ctrlKey || e.metaKey)
      zoom(Math.exp(-e.deltaY * 0.0015), e.clientX - r.left, e.clientY - r.top);
    else {
      state.view.x -= e.deltaX;
      state.view.y -= e.deltaY;
      transform();
      save();
    }
  },
  { passive: false },
);
$("#cards").addEventListener("keydown", (e) => {
  if (e.target.closest("button")) return;
  const el = e.target.closest(".agent-card");
  if (!el) return;
  const id = el.dataset.id;
  if (e.key === "Enter") {
    e.preventDefault();
    if (state.mode === "connect") connectNode(id);
    else openNode(id);
  } else if (e.key === " ") {
    e.preventDefault();
    e.stopPropagation();
    if (!e.shiftKey) state.selected.clear();
    state.selected.has(id) ? state.selected.delete(id) : state.selected.add(id);
    render();
  } else if (e.key.startsWith("Arrow")) {
    e.preventDefault();
    if (!state.selected.has(id)) {
      state.selected.clear();
      state.selected.add(id);
    }
    for (const key of state.selected) {
      const p = state.positions[key],
        step = e.shiftKey ? 50 : 10;
      if (e.key === "ArrowLeft") p.x -= step;
      if (e.key === "ArrowRight") p.x += step;
      if (e.key === "ArrowUp") p.y -= step;
      if (e.key === "ArrowDown") p.y += step;
    }
    render();
    save();
  }
});
$("#select-mode").onclick = () => {
  state.mode = "select";
  state.connectFrom = null;
  render();
};
$("#pan-mode").onclick = () => {
  state.mode = "pan";
  state.connectFrom = null;
  render();
};
$("#link-mode").onclick = () => {
  state.mode = "connect";
  state.connectFrom = null;
  render();
  toast("Select an agent, then a chat.");
};
$("#disconnect").onclick = async () => {
  const edge = state.edges.find((e) => e.id === state.selectedEdge);
  if (edge?.kind !== "chat") return;
  try {
    await api("/api/connections", {
      source: edge.source,
      target: edge.target,
      connected: false,
    });
    state.selectedEdge = null;
    await refresh();
    toast("Agent disconnected. Chat history stays available.");
  } catch (error) {
    toast(error.message);
  }
};
document.addEventListener("keyup", (e) => {
  if (e.key === " ") state.space = false;
});
window.addEventListener("blur", () => {
  state.space = false;
});
$("#detail-body").addEventListener("click", (e) => {
  const button = e.target.closest("[data-member]");
  if (button) openDetail("agent", button.dataset.member);
  const group = e.target.closest("[data-chat]");
  if (group) openDetail("chat", group.dataset.chat);
});
$("#detail-tabs").addEventListener("click", (e) => {
  if (e.target.dataset.tab) setTab(e.target.dataset.tab);
});
$("#search").addEventListener("input", (e) => {
  state.query = e.target.value.toLowerCase().trim();
  render();
  fit();
});
$$("[data-filter]").forEach(
  (b) =>
    (b.onclick = () => {
      state.filter = b.dataset.filter;
      $$("[data-filter]").forEach((x) =>
        x.classList.toggle("selected", x === b),
      );
      render();
      fit();
    }),
);
$("#clear-filter").onclick = () => {
  $("#search").value = "";
  state.query = "";
  $('[data-filter="all"]').click();
};
$("#arrange").onclick = () => {
  arrange(true);
  render();
  fit();
  toast("Visible agents arranged.");
};
$("#fit").onclick = fit;
$("#reset-view").onclick = fit;
$("#zoom-in").onclick = () => zoom(1.15);
$("#zoom-out").onclick = () => zoom(1 / 1.15);
$("#close-detail").onclick = closeDetail;
$("#canvas-view").onclick = closeDetail;
$("#resources").onclick = () => {
  state.opened = { kind: "resources" };
  state.detailKey = "";
  clearTimeout(detailTimer);
  $("#inspector").hidden = false;
  renderResources();
};
$("#help").onclick = () => $("#help-dialog").showModal();
$("#close-help").onclick = () => $("#help-dialog").close();
$("#show-tools").onchange = () => {
  state.detailKey = "";
  loadDetail();
};
$("#follow").onchange = () => {
  if ($("#follow").checked)
    $("#detail-body").scrollTop = $("#detail-body").scrollHeight;
};
$("#copy").onclick = async () => {
  try {
    await navigator.clipboard.writeText($("#detail-body").innerText);
    toast("Visible transcript copied.");
  } catch {
    toast("The browser could not access the clipboard.");
  }
};
function showGroup() {
  $("#chat-name").value = "";
  $("#chat-members").innerHTML = [...state.selected]
    .map((id) => nodes().find((n) => n.id === id))
    .filter((n) => n?.kind === "agent")
    .map((n) => `<span class="pill">${escape(n.name)}</span>`)
    .join("");
  $("#chat-dialog").showModal();
  $("#chat-name").focus();
}
$("#new-chat").onclick = showGroup;
$("#chat-selected").onclick = showGroup;
$("#clear-selection").onclick = () => {
  state.selected.clear();
  state.selectedEdge = null;
  render();
};
$("#cancel-chat").onclick = () => $("#chat-dialog").close();
$("#chat-form").onsubmit = async (e) => {
  e.preventDefault();
  if (state.creatingGroup) return;
  state.creatingGroup = true;
  const button = e.submitter;
  button.disabled = true;
  const name = $("#chat-name").value;
  const members = [...state.selected]
    .filter((id) => state.threads.some((t) => t.id === id))
    .sort();
  if (
    !state.pendingGroup ||
    state.pendingGroup.name !== name ||
    JSON.stringify(state.pendingGroup.members) !== JSON.stringify(members)
  ) {
    state.pendingGroup = { id: crypto.randomUUID(), name, members };
  }
  try {
    const group = await api("/api/chats", state.pendingGroup);
    state.pendingGroup = null;
    $("#chat-dialog").close();
    state.selected.clear();
    await refresh();
    openDetail("chat", group.id);
    toast("Chat node created. Connect agents through its port.");
  } catch (error) {
    toast(error.message);
  } finally {
    state.creatingGroup = false;
    button.disabled = false;
  }
};
$("#composer").onsubmit = async (e) => {
  e.preventDefault();
  const room = state.opened?.id,
    text = $("#message").value.trim();
  if (!text || !room || state.sending) return;
  state.sending = true;
  meta();
  const current = state.pendingSend;
  if (!current || current.room !== room || current.text !== text)
    state.pendingSend = { id: crypto.randomUUID(), room, text };
  try {
    const result = await api("/api/messages", state.pendingSend);
    state.pendingSend = null;
    const unresolved = Object.values(result.deliveries).filter(
      (v) => v !== "queued",
    ).length;
    toast(
      unresolved
        ? `Saved. ${unresolved} deliveries are not confirmed. Read Chat for details.`
        : "Message saved and queued.",
    );
    if (state.opened?.id === room) {
      if ($("#message").value.trim() === text) $("#message").value = "";
      setTab("chat");
    }
  } catch (error) {
    toast(error.message);
  } finally {
    state.sending = false;
    meta();
  }
};
$("#message").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
    e.preventDefault();
    $("#composer").requestSubmit();
  }
});
document.addEventListener("keydown", (e) => {
  if (e.target.closest("input,textarea,dialog")) return;
  if (e.key === "/") {
    e.preventDefault();
    $("#search").focus();
  }
  if (e.key.toLowerCase() === "f") fit();
  if (e.key.toLowerCase() === "g") showGroup();
  if (e.key === "Escape") {
    closeDetail();
    state.selected.clear();
    state.selectedEdge = null;
    state.connectFrom = null;
    render();
  }
  if (e.key === " ") {
    e.preventDefault();
    state.space = true;
  }
  if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "a") {
    e.preventDefault();
    state.selected = new Set(visible().map((n) => n.id));
    render();
  }
});
new ResizeObserver(() => transform()).observe($("#viewport"));
refresh();

function renderChats() {
  if (state.opened?.kind !== "chats") return;
  $("#detail-label").textContent = "TEAM CONVERSATIONS";
  $("#detail-title").textContent = "Chats";
  $("#detail-meta").textContent = "Chat nodes and their current connections";
  $("#detail-tabs").hidden = true;
  $("#stream-controls").hidden = true;
  $("#composer").hidden = true;
  const groups = state.chats;
  const html = groups
    .map(
      (g) =>
        `<button class="member-button" data-chat="${escape(g.id)}">${escape(g.name)}<small>${g.members.length} members · Chat node</small></button>`,
    )
    .join("");
  setBody(html || '<p class="notice">No group chats yet.</p>', html);
}
$("#chats").onclick = () => {
  state.opened = { kind: "chats" };
  state.detailKey = "";
  clearTimeout(detailTimer);
  $("#inspector").hidden = false;
  renderChats();
};
