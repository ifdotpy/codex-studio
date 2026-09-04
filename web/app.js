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
  groups: [],
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
  state.storageKey = `codex-canvas:${key}`;
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
function visible() {
  return state.threads.filter(
    (t) =>
      (state.filter === "all" ||
        (state.filter === "active" ? active : attention).has(t.status)) &&
      [t.name, t.wave, t.branch, t.cwd, t.status, t.model].some((v) =>
        String(v || "")
          .toLowerCase()
          .includes(state.query),
      ),
  );
}
function ownerGroup(id) {
  return (
    state.groups.find((g) => !g.automatic && g.members.includes(id)) ||
    state.groups.find((g) => g.automatic && g.members.includes(id))
  );
}
function groupLayout() {
  const map = new Map();
  for (const t of visible()) {
    const g = ownerGroup(t.id);
    if (!g) continue;
    if (!map.has(g.id)) map.set(g.id, { ...g, agents: [] });
    map.get(g.id).agents.push(t);
  }
  return [...map.values()].sort(
    (a, b) =>
      Math.min(
        ...a.agents.map((t) =>
          active.has(t.status) ? 0 : attention.has(t.status) ? 1 : 2,
        ),
      ) -
      Math.min(
        ...b.agents.map((t) =>
          active.has(t.status) ? 0 : attention.has(t.status) ? 1 : 2,
        ),
      ),
  );
}
function arrange(reset = false) {
  let x = 35,
    y = 45,
    rowHeight = 0;
  const columns = Math.max(
    1,
    Math.floor(($("#viewport").clientWidth - 100) / 360),
  );
  let col = 0;
  for (const g of groupLayout()) {
    const wide = g.agents.length > 2 ? 2 : 1;
    g.agents.forEach((t, i) => {
      if (reset || !state.positions[t.id])
        state.positions[t.id] = {
          x: x + 26 + (i % wide) * 315,
          y: y + 56 + Math.floor(i / wide) * 244,
        };
    });
    const height = 84 + Math.ceil(g.agents.length / wide) * 244;
    rowHeight = Math.max(rowHeight, height);
    col += wide;
    x += wide * 315 + 46;
    if (col >= columns) {
      x = 35;
      y += rowHeight + 30;
      col = 0;
      rowHeight = 0;
    }
  }
  save();
}
function bounds(threads = visible()) {
  if (!threads.length) return { x: 0, y: 0, w: 700, h: 400 };
  const ps = threads.map((t) => state.positions[t.id]).filter(Boolean);
  return {
    x: Math.min(...ps.map((p) => p.x)) - 26,
    y: Math.min(...ps.map((p) => p.y)) - 56,
    w:
      Math.max(...ps.map((p) => p.x + 292)) -
      Math.min(...ps.map((p) => p.x)) +
      52,
    h:
      Math.max(...ps.map((p) => p.y + 218)) -
      Math.min(...ps.map((p) => p.y)) +
      85,
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
    ctx.fillRect(ox + p.x * z, oy + p.y * z, 292 * z, 218 * z);
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
function renderGroups() {
  const container = $("#groups");
  container.replaceChildren();
  for (const g of groupLayout()) {
    const b = bounds(g.agents),
      el = document.createElement("div");
    el.className = `group-boundary${g.automatic ? "" : " custom"}`;
    Object.assign(el.style, {
      left: `${b.x}px`,
      top: `${b.y}px`,
      width: `${b.w}px`,
      height: `${b.h}px`,
    });
    el.innerHTML = `<div class="group-heading"><span class="group-name" title="${escape(g.name)}">${g.automatic ? "◌" : "◎"} &nbsp;${escape(g.name)}</span><span class="group-size">${g.agents.length} AGENT${g.agents.length === 1 ? "" : "S"}</span><button data-chat="${g.id}">Chat ↗</button></div>`;
    container.append(el);
  }
}
function card(t) {
  return `<div class="card-top"><div class="avatar">${escape(
    t.name
      .split("-")
      .slice(0, 2)
      .map((s) => s[0])
      .join("")
      .toUpperCase(),
  )}</div><div class="card-identity"><h3 title="${escape(t.name)}">${escape(t.name)}</h3><p>${escape(t.role || "Agent")} · ${escape(t.model || t.requestedModel || "Unknown model")}</p></div><span class="drag-grip">⠿</span></div><div class="card-status"><span class="status-dot"></span>${escape(label(t))}<time>${age(t.lastEvent)}</time></div><div class="card-tail">${escape(t.error || t.tail || "No agent message yet.")}</div><div class="card-footer"><span><strong>${number(t.tokensUsed)}</strong> goal tokens</span><span><strong>${number(t.events)}</strong> events</span><span>Open ↗</span></div>`;
}
function render() {
  if (state.drag) return;
  arrange();
  const threads = visible(),
    ids = new Set(threads.map((t) => t.id));
  for (const el of $$("#cards > *")) if (!ids.has(el.dataset.id)) el.remove();
  for (const t of threads) {
    let el = document.getElementById(`agent-${t.id}`);
    if (!el) {
      el = document.createElement("article");
      el.id = `agent-${t.id}`;
      el.dataset.id = t.id;
      el.tabIndex = 0;
      el.setAttribute("role", "button");
      $("#cards").append(el);
    }
    el.className = `agent-card${state.selected.has(t.id) ? " chosen" : ""}${state.opened?.id === t.id ? " open" : ""}`;
    el.dataset.tone = tone(t);
    el.setAttribute("aria-label", `${t.name}, ${label(t)}. Open details.`);
    el.setAttribute("aria-pressed", String(state.selected.has(t.id)));
    const html = card(t);
    if (el.innerHTML !== html) el.innerHTML = html;
    const p = state.positions[t.id];
    el.style.left = `${p.x}px`;
    el.style.top = `${p.y}px`;
  }
  renderGroups();
  transform();
  $("#empty").hidden = threads.length > 0;
  $("#empty h2").textContent = state.threads.length
    ? "No matching agents"
    : "No agents here yet";
  $("#empty p").textContent = state.threads.length
    ? "Change the search or filter to see more agents."
    : "Start a wave with codex-agents. Its agents appear here automatically.";
  $("#clear-filter").hidden = !state.threads.length;
  $("#all-count").textContent = state.threads.length;
  $("#active-count").textContent = state.threads.filter((t) =>
    active.has(t.status),
  ).length;
  $("#attention-count").textContent = state.threads.filter((t) =>
    attention.has(t.status),
  ).length;
  $("#health").innerHTML =
    `<span><strong>${state.threads.filter((t) => active.has(t.status)).length}</strong> active</span><span><strong>${state.groups.filter((g) => g.automatic).length}</strong> waves</span>`;
  $("#total-tokens").textContent =
    `${number(state.threads.reduce((n, t) => n + (t.tokensUsed || 0), 0))} goal tokens`;
  $("#caption").textContent =
    `${threads.length} AGENTS · ${groupLayout().length} GROUPS`;
  $("#selection-bar").hidden = !state.selected.size;
  $("#selection-count").textContent = `${state.selected.size} selected`;
  $("#group-selected").disabled = state.selected.size < 2;
}
async function refresh() {
  clearTimeout(refreshTimer);
  try {
    const result = await api("/api/state");
    const first = !state.storageKey;
    if (first) loadLayout(result.stateDir);
    Object.assign(state, {
      threads: result.threads,
      groups: result.groups,
      board: result.board,
      boardError: result.boardError,
      token: result.token,
    });
    state.selected = new Set(
      [...state.selected].filter((id) =>
        state.threads.some((t) => t.id === id),
      ),
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
  state.tab = kind === "group" ? "chat" : "activity";
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
    g = state.groups.find((g) => g.id === o.id),
    isGroup = o.kind === "group";
  $("#detail-label").textContent = isGroup
    ? "SHARED GROUP CHAT"
    : "AGENT / " + (t?.role || "worker").toUpperCase();
  $("#detail-title").textContent = isGroup
    ? g?.name || "Group unavailable"
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
    ? "Message all group members"
    : "Message this agent";
  const canSend = isGroup
    ? !!g?.members.some((id) => state.threads.find((t) => t.id === id)?.canSend)
    : !!t?.canSend;
  $("#composer button").disabled = !canSend || state.sending;
  $("#message").disabled = !canSend || state.sending;
  $("#message").placeholder = canSend
    ? "Send an instruction…"
    : "The launcher is offline";
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
      const g = state.groups.find((g) => g.id === opened.id),
        t = state.threads.find((t) => t.id === opened.id);
      if (opened.kind === "group") {
        setBody(
          `<p class="notice">Members keep their original run identity. A replacement run does not receive messages for this group.</p>${(
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
      const g = state.groups.find((g) => g.id === opened.id);
      const html =
        (g
          ? '<p class="notice">Your messages go to all members. Agents can read and post here with codex-chat. Peer posts do not start turns automatically.</p>'
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
  if (e.target.closest("button,#minimap,.canvas-controls,#selection-bar"))
    return;
  const card = e.target.closest(".agent-card");
  if (e.button !== 0) return;
  state.drag = {
    pointer: e.pointerId,
    startX: e.clientX,
    startY: e.clientY,
    id: card?.dataset.id,
    position: card
      ? { ...state.positions[card.dataset.id] }
      : { ...state.view },
    moved: false,
    shift: e.shiftKey,
  };
  $("#viewport").setPointerCapture(e.pointerId);
  $("#viewport").classList.add("dragging");
});
$("#viewport").addEventListener("pointermove", (e) => {
  const d = state.drag;
  if (!d || d.pointer !== e.pointerId) return;
  const dx = e.clientX - d.startX,
    dy = e.clientY - d.startY;
  if (Math.hypot(dx, dy) > 4) d.moved = true;
  if (!d.moved) return;
  if (d.id) {
    const p = {
      x: d.position.x + dx / state.view.z,
      y: d.position.y + dy / state.view.z,
    };
    state.positions[d.id] = p;
    const el = document.getElementById(`agent-${d.id}`);
    el.style.left = `${p.x}px`;
    el.style.top = `${p.y}px`;
    renderGroups();
    drawMinimap();
  } else {
    state.view.x = d.position.x + dx;
    state.view.y = d.position.y + dy;
    transform();
  }
});
function finishDrag(e, cancel = false) {
  const d = state.drag;
  if (!d || d.pointer !== e.pointerId) return;
  state.drag = null;
  $("#viewport").classList.remove("dragging");
  if (!cancel && !d.moved && d.id) {
    if (d.shift) {
      state.selected.has(d.id)
        ? state.selected.delete(d.id)
        : state.selected.add(d.id);
      render();
    } else openDetail("agent", d.id);
  } else {
    save();
    render();
  }
}
$("#viewport").addEventListener("pointerup", (e) => finishDrag(e));
$("#viewport").addEventListener("pointercancel", (e) => finishDrag(e, true));
$("#viewport").addEventListener(
  "wheel",
  (e) => {
    e.preventDefault();
    const r = $("#viewport").getBoundingClientRect();
    zoom(Math.exp(-e.deltaY * 0.0015), e.clientX - r.left, e.clientY - r.top);
  },
  { passive: false },
);
$("#cards").addEventListener("keydown", (e) => {
  const el = e.target.closest(".agent-card");
  if (!el) return;
  const id = el.dataset.id;
  if (["Enter", " "].includes(e.key)) {
    e.preventDefault();
    if (e.shiftKey) {
      state.selected.has(id)
        ? state.selected.delete(id)
        : state.selected.add(id);
      render();
    } else openDetail("agent", id);
  } else if (e.key.startsWith("Arrow")) {
    e.preventDefault();
    const p = state.positions[id],
      step = e.shiftKey ? 50 : 10;
    if (e.key === "ArrowLeft") p.x -= step;
    if (e.key === "ArrowRight") p.x += step;
    if (e.key === "ArrowUp") p.y -= step;
    if (e.key === "ArrowDown") p.y += step;
    render();
    save();
  }
});
$("#groups").addEventListener("click", (e) => {
  const button = e.target.closest("[data-chat]");
  if (button) openDetail("group", button.dataset.chat);
});
$("#detail-body").addEventListener("click", (e) => {
  const button = e.target.closest("[data-member]");
  if (button) openDetail("agent", button.dataset.member);
  const group = e.target.closest("[data-group]");
  if (group) openDetail("group", group.dataset.group);
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
  if (state.selected.size < 2) {
    toast("Select at least two agents with Shift + click.");
    return;
  }
  $("#group-name").value = "";
  $("#group-members").innerHTML = [...state.selected]
    .map(
      (id) =>
        `<span class="pill">${escape(state.threads.find((t) => t.id === id)?.name)}</span>`,
    )
    .join("");
  $("#group-dialog").showModal();
  $("#group-name").focus();
}
$("#new-group").onclick = showGroup;
$("#group-selected").onclick = showGroup;
$("#clear-selection").onclick = () => {
  state.selected.clear();
  render();
};
$("#cancel-group").onclick = () => $("#group-dialog").close();
$("#group-form").onsubmit = async (e) => {
  e.preventDefault();
  if (state.creatingGroup) return;
  state.creatingGroup = true;
  const button = e.submitter;
  button.disabled = true;
  const name = $("#group-name").value;
  const members = [...state.selected].sort();
  if (
    !state.pendingGroup ||
    state.pendingGroup.name !== name ||
    JSON.stringify(state.pendingGroup.members) !== JSON.stringify(members)
  ) {
    state.pendingGroup = { id: crypto.randomUUID(), name, members };
  }
  try {
    const group = await api("/api/groups", state.pendingGroup);
    state.pendingGroup = null;
    $("#group-dialog").close();
    state.selected.clear();
    await refresh();
    openDetail("group", group.id);
    toast("Group created. Its chat is ready.");
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
    render();
  }
});
new ResizeObserver(() => transform()).observe($("#viewport"));
refresh();

function renderChats() {
  if (state.opened?.kind !== "chats") return;
  $("#detail-label").textContent = "TEAM CONVERSATIONS";
  $("#detail-title").textContent = "Group chats";
  $("#detail-meta").textContent =
    "All waves and groups, including earlier members";
  $("#detail-tabs").hidden = true;
  $("#stream-controls").hidden = true;
  $("#composer").hidden = true;
  const groups = [...state.groups].sort(
    (a, b) => Number(a.automatic) - Number(b.automatic),
  );
  const html = groups
    .map(
      (g) =>
        `<button class="member-button" data-group="${escape(g.id)}">${escape(g.name)}<small>${g.members.length} members · ${g.automatic ? "Wave" : "Custom group"}</small></button>`,
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
