const $ = (s) => document.querySelector(s);
const esc = (s) =>
  String(s ?? "").replace(
    /[&<>"']/g,
    (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[
        c
      ],
  );
const busy = new Set(["running", "starting", "approval"]);
const status = (a) =>
  ({
    idle: "Ready",
    running: "Working",
    starting: "Starting",
    queued: "Queued",
    waiting: "Waiting for results",
    completed: "Complete",
    failed: "Failed",
    paused: "Stopped",
    approval: "Needs an answer",
    interrupted: "Interrupted",
  })[a?.status] ||
  a?.status ||
  "";
const state = {
  threads: [],
  chats: [],
  edges: [],
  runtime: null,
  token: "",
  lead: null,
  opened: null,
  view: "chat",
  drafts: new Map(),
  sends: new Map(),
  creating: false,
  sending: false,
  pendingLead: null,
  positions: {},
  camera: { x: 40, y: 40, z: 1 },
  layoutKey: null,
  autoFollow: true,
  items: [],
  messageKey: "",
  drag: null,
  request: null,
  query: "",
  workerQuery: "",
};
let pollTimer, detailTimer, toastTimer;
const current = () => state.threads.find((a) => a.id === state.opened);
const lead = () => state.threads.find((a) => a.id === state.lead);
const leads = () =>
  state.threads.filter((a) => a.source === "managed" && a.isLead === true);
const members = () => state.threads.filter((a) => a.rootId === state.lead);
const isGroup = () => state.chats.find((a) => a.id === state.opened);
function toast(text) {
  $("#toast").textContent = text;
  $("#toast").hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => ($("#toast").hidden = true), 4500);
}
async function api(path, body) {
  const response = await fetch(
    path,
    body === undefined
      ? {}
      : {
          method: "POST",
          headers: {
            "Content-Type": "application/json",
            "X-Canvas-Token": state.token,
          },
          body: JSON.stringify(body),
        },
  );
  const result = await response.json();
  if (!response.ok)
    throw Error(result.error || `Request failed (${response.status})`);
  return result;
}
function updateHTML(element, html) {
  if (element.innerHTML !== html) element.innerHTML = html;
}
function rememberDraft() {
  if (state.opened) state.drafts.set(state.opened, $("#message").value);
}
function openConversation(id) {
  rememberDraft();
  state.opened = id;
  const a = current();
  if (a?.isLead) state.lead = a.id;
  else if (a?.rootId && leads().some((l) => l.id === a.rootId))
    state.lead = a.rootId;
  else state.lead = null;
  state.messageKey = "";
  state.autoFollow = true;
  $("#message").value = state.drafts.get(id) || "";
  resizeInput();
  $("#conversation-menu").open = false;
  document.body.classList.remove("sidebar-open", "team-open");
  setView("chat");
  render();
  loadMessages();
}
async function newChat() {
  if (state.creating) return;
  state.creating = true;
  $("#new-chat").disabled = true;
  state.pendingLead ||= {
    id: crypto.randomUUID(),
    previous: lead()?.id || null,
    model: $("#model").value,
  };
  try {
    const a = await api("/api/leads", state.pendingLead);
    state.pendingLead = null;
    await refresh();
    openConversation(a.id);
    $("#message").focus();
  } catch (error) {
    toast(error.message);
  } finally {
    state.creating = false;
    $("#new-chat").disabled = false;
  }
}
async function refresh() {
  clearTimeout(pollTimer);
  try {
    const data = await api("/api/state");
    if (state.closed) return;
    Object.assign(state, {
      threads: data.threads,
      chats: data.chats,
      edges: data.edges,
      runtime: data.runtime,
      token: data.token,
    });
    if (!state.layoutKey) {
      state.layoutKey = `codex-canvas-graph:${data.stateDir}`;
      try {
        const saved = JSON.parse(localStorage.getItem(state.layoutKey));
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
            state.camera = {
              ...saved.view,
              z: Math.max(0.15, Math.min(2, saved.view.z)),
            };
        }
      } catch {
        /* Invalid browser preferences do not affect server state. */
      }
    }
    $("#error").hidden = true;
    if (!state.opened && leads().length) openConversation(leads().at(-1).id);
    render();
  } catch (error) {
    $("#error").hidden = false;
    $("#error").textContent = `Connection lost. ${error.message}`;
  } finally {
    if (!state.closed) pollTimer = setTimeout(refresh, 1600);
  }
}
function render() {
  const a = current(),
    root = lead(),
    group = isGroup();
  updateHTML(
    $("#chat-list"),
    leads()
      .filter((a) => a.name.toLowerCase().includes(state.query))
      .slice()
      .reverse()
      .map(
        (a) =>
          `<button class="chat-row ${state.lead === a.id ? "selected" : ""}" data-chat="${esc(a.id)}" aria-current="${state.lead === a.id}">${esc(a.name)}${busy.has(a.status) ? `<small>${esc(status(a))}</small>` : ""}</button>`,
      )
      .join("") || '<p class="notice">Your conversations appear here.</p>',
  );
  $("#conversation-title").textContent =
    a?.name || group?.name || "New conversation";
  $("#conversation-title").title = a?.name || group?.name || "";
  $("#conversation-status").textContent = status(a);
  $("#back-lead").hidden = !root || root.id === a?.id;
  $("#model").hidden = !!a && !a.isLead;
  if (a?.isLead) $("#model").value = a.model;
  $("#model").disabled = !!a && (busy.has(a.status) || a.inFlight);
  $("#conversation-menu").hidden = !a || a.source !== "managed";
  $("#view-toggle").disabled = !a;
  $("#stop").hidden = !a || a.source !== "managed" || !busy.has(a.status);
  $("#project").textContent =
    a?.cwd?.split("/").filter(Boolean).at(-1) || "Project";
  $("#project").title = a?.cwd || "Project directory";
  $("#project").disabled = !a?.cwd;
  const canSend = a?.canSend || !!group || !state.opened;
  $("#message").disabled = !canSend;
  $("#message").placeholder = canSend
    ? a && !a.isLead
      ? `Message ${a.name}…`
      : "What should we work on?"
    : "This session has no live mailbox";
  $("#send").disabled = state.sending || !canSend;
  renderTeam();
  renderRequests();
  if (state.view === "canvas") renderCanvas();
  if (!state.opened) showMessages([]);
}
function renderTeam() {
  const team = members(),
    workers = team.filter((a) => a.id !== state.lead),
    monitors = (state.runtime?.monitors || []).filter((m) =>
      team.some((a) => a.id === m.agent),
    );
  $("#team").hidden = !workers.length && !monitors.length;
  $("#team-toggle").hidden = $("#team").hidden;
  $("#team-count").textContent =
    `${workers.filter((a) => busy.has(a.status)).length} active / ${workers.length}`;
  $("#lead-row").textContent = lead()?.name || "Lead";
  $("#worker-search").hidden = workers.length < 8;
  const shown = workers.filter((a) =>
    [a.name, a.status, a.role].some((s) =>
      String(s || "")
        .toLowerCase()
        .includes(state.workerQuery),
    ),
  );
  const rank = (a) =>
    ["failed", "approval", "interrupted"].includes(a.status)
      ? 0
      : busy.has(a.status)
        ? 1
        : 2;
  const row = (a) =>
    `<button class="worker ${state.opened === a.id ? "selected" : ""}" data-worker="${esc(a.id)}"><span class="dot ${esc(a.status)}"></span><span class="worker-text"><strong>${esc(a.name)}</strong><small>${esc(status(a))}</small></span></button>`;
  const ongoing = shown
    .filter((a) => a.status !== "completed")
    .sort((a, b) => rank(a) - rank(b) || a.name.localeCompare(b.name));
  const done = shown.filter((a) => a.status === "completed");
  const doneOpen = $("#workers .worker-group")?.open;
  updateHTML(
    $("#workers"),
    ongoing.map(row).join("") +
      (done.length
        ? `<details class="worker-group" ${doneOpen || state.workerQuery ? "open" : ""}><summary>Completed · ${done.length}</summary>${done.map(row).join("")}</details>`
        : "") || '<p class="notice">No matching workers.</p>',
  );
  const html = monitors
    .slice(-15)
    .reverse()
    .map(
      (m) =>
        `<details data-watch="${esc(m.id)}"><summary>⌘ ${esc(m.command.slice(0, 50))} · ${esc(m.status)}</summary><pre class="tool-output">${esc(m.command)}\n\n${esc(m.tail || m.error || "No output yet")}\n\nExit: ${m.exitCode ?? "pending"}</pre>${["running", "starting", "approval"].includes(m.status) ? `<button data-cancel-watch="${esc(m.id)}">Cancel command</button>` : ""}</details>`,
    )
    .join("");
  preserveDetails($("#monitors"), html);
}
function safeMarkdown(text) {
  return DOMPurify.sanitize(marked.parse(text, { gfm: true, breaks: false }), {
    USE_PROFILES: { html: true },
    FORBID_TAGS: ["img", "style", "form", "input", "button", "iframe"],
    FORBID_ATTR: ["style", "id", "name"],
  });
}
function preserveDetails(el, html) {
  if (el.innerHTML === html) return;
  const opened = new Set(
    [...el.querySelectorAll("details[open]")].map(
      (d) => d.dataset.key || d.dataset.watch,
    ),
  );
  el.innerHTML = html;
  for (const d of el.querySelectorAll("details"))
    if (opened.has(d.dataset.key || d.dataset.watch)) d.open = true;
}
function showMessages(items, notice = "") {
  const el = $("#messages");
  const key = JSON.stringify(items) + notice + state.opened;
  if (key === state.messageKey) return;
  state.messageKey = key;
  state.items = items;
  const scroll = el.scrollTop;
  let html = notice ? `<p class="notice">${esc(notice)}</p>` : "";
  for (let i = 0; i < items.length; i++) {
    const item = items[i];
    if (["tool", "output"].includes(item.role)) {
      const batch = [item];
      while (
        i + 1 < items.length &&
        ["tool", "output"].includes(items[i + 1].role)
      )
        batch.push(items[++i]);
      html += `<details class="tool-group" data-key="${esc(item.id)}"><summary>${batch.length} action${batch.length === 1 ? "" : "s"}</summary>${batch.map((t) => `<details data-key="${esc(t.id)}"><summary>${esc(t.title || "Tool")}</summary><pre class="tool-output">${esc(t.text)}</pre></details>`).join("")}</details>`;
    } else {
      html += `<article class="message ${item.role === "user" ? "user" : "assistant"}" data-message="${esc(item.id)}">${item.role === "user" && item.pending ? '<span class="message-label">Queued</span>' : ""}<div class="prose">${item.role === "user" ? esc(item.text) : safeMarkdown(item.text)}</div>${item.truncated ? '<p class="notice">This message is clipped.</p>' : ""}<button class="copy-message" data-copy="${esc(item.id)}" aria-label="Copy message">Copy</button></article>`;
    }
  }
  if (!html)
    html = `<div class="empty-chat"><h2>What should we work on?</h2><p>${state.opened ? "Give your lead the task. It can delegate work and bring the results back here." : "Start a conversation. Your lead can coordinate the team."}</p></div>`;
  preserveDetails(el, html);
  for (const a of el.querySelectorAll("a")) {
    a.target = "_blank";
    a.rel = "noopener noreferrer";
  }
  if (state.autoFollow) el.scrollTop = el.scrollHeight;
  else el.scrollTop = scroll;
}
async function loadMessages() {
  clearTimeout(detailTimer);
  const id = state.opened;
  if (!id) return;
  try {
    if (isGroup()) {
      const messages = await api(
        `/api/messages?room=${encodeURIComponent(id)}`,
      );
      if (state.opened !== id) return;
      showMessages(
        messages.map((m) => ({
          ...m,
          role: m.author === "user" ? "user" : "assistant",
        })),
      );
    } else {
      const data = await api(`/api/transcript?id=${encodeURIComponent(id)}`);
      if (state.opened !== id) return;
      showMessages(
        data.items || [],
        data.unavailable ||
          (data.truncated
            ? "Recent messages only. The complete transcript remains on disk."
            : ""),
      );
    }
  } catch (error) {
    if (state.opened === id) toast(error.message);
  } finally {
    if (state.opened === id) detailTimer = setTimeout(loadMessages, 900);
  }
}
async function sendMessage(event) {
  event.preventDefault();
  const text = $("#message").value.trim();
  if (!text || state.sending) return;
  if (!state.opened) {
    await newChat();
    if (!state.opened) return;
    $("#message").value = text;
  }
  const id = state.opened;
  state.sending = true;
  $("#send-state").textContent = "Sending…";
  render();
  try {
    if (
      current()?.source === "managed" &&
      /^\/(monitor|compact|review|stop|stop-team)(\s|$)/.test(text)
    ) {
      const [command, ...rest] = text.split(" ");
      if (command === "/monitor")
        await api("/api/monitor", {
          id: crypto.randomUUID(),
          agent: id,
          command: rest.join(" "),
        });
      else if (command === "/stop" || command === "/stop-team")
        await api("/api/stop", {
          id: command === "/stop-team" ? current().rootId : id,
          descendants: command === "/stop-team",
        });
      else await api("/api/action", { id, action: command.slice(1) });
    } else {
      const previous = state.sends.get(id);
      if (!previous || previous.text !== text)
        state.sends.set(id, { id: crypto.randomUUID(), room: id, text });
      const result = await api("/api/messages", state.sends.get(id));
      state.sends.delete(id);
      if (Object.values(result.deliveries || {}).some((v) => v !== "queued"))
        toast("Message saved. Some deliveries are not confirmed.");
    }
    if (state.drafts.get(id)?.trim() === text) state.drafts.delete(id);
    if (state.opened === id && $("#message").value.trim() === text)
      $("#message").value = "";
    state.autoFollow = true;
    resizeInput();
    await refresh();
    await loadMessages();
  } catch (error) {
    toast(error.message);
  } finally {
    state.sending = false;
    $("#send-state").textContent = "";
    render();
  }
}
function resizeInput() {
  $("#message").style.height = "auto";
  $("#message").style.height = `${Math.min(220, $("#message").scrollHeight)}px`;
}
function renderRequests() {
  const team = new Set(members().map((a) => a.id));
  team.add(state.opened);
  const requests = (state.runtime?.requests || []).filter(
    (r) => !r.agent || team.has(r.agent),
  );
  const html = requests
    .map((r) => {
      const p = r.params || {},
        name = state.threads.find((a) => a.id === r.agent)?.name || "Codex";
      const question =
        ["item/tool/requestUserInput", "agent/asyncQuestion"].includes(
          r.method,
        ) ||
        (r.method === "mcpServer/elicitation/request" && p.mode === "form");
      const approval =
        [
          "monitor/approve",
          "item/commandExecution/requestApproval",
          "item/fileChange/requestApproval",
          "item/permissions/requestApproval",
          "applyPatchApproval",
          "execCommandApproval",
        ].includes(r.method) ||
        (r.method === "mcpServer/elicitation/request" && p.mode === "url");
      const command = p.command || r.preview?.command;
      return `<div class="request"><strong>${esc(name)}</strong><p>${esc(p.reason || p.message || (question ? "The agent has a question." : "Approval required."))}</p>${command ? `<pre>${esc(Array.isArray(command) ? command.join(" ") : command)}</pre>` : ""}${p.cwd ? `<p>${esc(p.cwd)}</p>` : ""}${p.permissions || r.preview?.changes ? `<pre>${esc(JSON.stringify(p.permissions || r.preview.changes, null, 2))}</pre>` : ""}${p.url && /^https?:\/\//i.test(p.url) ? `<a href="${esc(p.url)}" target="_blank" rel="noreferrer">Open request</a>` : ""}${question ? `<button data-answer="${esc(r.id)}">Answer</button>` : approval ? `<button data-approve="${esc(r.id)}" data-decision="accept">Approve</button><button data-approve="${esc(r.id)}" data-decision="decline">Decline</button>` : `<p>Client request: ${esc(r.method)}. This client cannot provide it.</p>`}</div>`;
    })
    .join("");
  updateHTML($("#requests"), html);
  $("#requests").hidden = !requests.length;
}
function answerDialog(id) {
  const r = state.runtime.requests.find((r) => r.id === id);
  if (!r) return;
  state.request = r;
  const questions =
    r.params.questions ||
    Object.entries(r.params.requestedSchema?.properties || {}).map(
      ([id, p]) => ({
        id,
        question: p.title || id,
        options: p.enum?.map((label) => ({ label })),
        schema: p,
      }),
    );
  $("#answer-fields").innerHTML = questions
    .map(
      (q, i) =>
        `<label>${esc(q.question)}${q.options?.length ? `<select data-question="${esc(q.id)}"><option value="">Choose an answer</option>${q.options.map((o) => `<option value="${esc(o.label)}">${esc(o.label)}${o.description ? " · " + esc(o.description) : ""}</option>`).join("")}</select>` : ""}<input data-answer-text="${esc(q.id)}" placeholder="${q.options?.length ? "Or write an answer" : "Your answer"}" ${q.isSecret ? 'type="password"' : ""} /></label>`,
    )
    .join("");
  $("#answer-dialog").showModal();
}
async function submitAnswer(event) {
  event.preventDefault();
  const r = state.request;
  if (!r) return;
  try {
    const values = {};
    for (const input of $("#answer-fields").querySelectorAll(
      "[data-answer-text]",
    )) {
      const id = input.dataset.answerText;
      const select = [...$("#answer-fields").querySelectorAll("select")].find(
        (e) => e.dataset.question === id,
      );
      values[id] = input.value || select?.value || "";
    }
    let body;
    if (
      ["item/tool/requestUserInput", "agent/asyncQuestion"].includes(r.method)
    )
      body = {
        answers: Object.fromEntries(
          Object.entries(values).map(([k, v]) => [k, { answers: [v] }]),
        ),
      };
    else {
      const schema = r.params.requestedSchema;
      const content = {};
      for (const [k, v] of Object.entries(values)) {
        const t = schema.properties[k].type;
        content[k] =
          t === "boolean"
            ? v === "true"
            : ["integer", "number"].includes(t)
              ? Number(v)
              : ["object", "array"].includes(t)
                ? JSON.parse(v)
                : v;
      }
      body = { decision: "accept", content };
    }
    await api("/api/answer", { id: r.id, ...body });
    $("#answer-dialog").close();
    await refresh();
  } catch (error) {
    toast(error.message);
  }
}
function setView(view) {
  state.view = view;
  $("#canvas").hidden = view !== "canvas";
  $("#conversation").hidden = view === "canvas";
  $("#view-toggle").textContent = view === "canvas" ? "Chat" : "Canvas";
  $("#view-toggle").setAttribute("aria-pressed", String(view === "canvas"));
  if (view === "canvas") renderCanvas();
}
function canvasAgents() {
  return lead() ? members() : state.threads;
}
function renderCanvas() {
  if (state.drag) return;
  const agents = canvasAgents();
  agents.forEach((a, i) => {
    if (!state.positions[a.id])
      state.positions[a.id] = { x: (i % 4) * 270, y: Math.floor(i / 4) * 140 };
  });
  updateHTML(
    $("#nodes"),
    agents
      .map(
        (a) =>
          `<button class="node ${a.id === state.opened ? "selected" : ""}" data-node="${esc(a.id)}" style="left:${state.positions[a.id].x}px;top:${state.positions[a.id].y}px"><span class="dot ${esc(a.status)}"></span><span><strong>${esc(a.name)}</strong><small>${a.isLead ? "Lead · " : ""}${esc(status(a))}</small></span></button>`,
      )
      .join(""),
  );
  const ids = new Set(agents.map((a) => a.id));
  updateHTML(
    $("#edges"),
    agents
      .filter((a) => ids.has(a.parentId))
      .map((a) => {
        const p = state.positions[a.parentId],
          q = state.positions[a.id],
          x = p.x + 210,
          y = p.y + 38;
        return `<path d="M${x},${y} C${x + 35},${y} ${q.x - 35},${q.y + 38} ${q.x},${q.y + 38}" />`;
      })
      .join(""),
  );
  transformCanvas();
}
function transformCanvas() {
  const { x, y, z } = state.camera;
  $("#world").style.transform = `translate(${x}px,${y}px) scale(${z})`;
}
function saveLayout() {
  try {
    localStorage.setItem(
      state.layoutKey,
      JSON.stringify({ positions: state.positions, view: state.camera }),
    );
  } catch {
    toast("The browser could not save canvas positions.");
  }
}
function fitCanvas() {
  const positions = canvasAgents().map((a) => state.positions[a.id]);
  if (!positions.length) return;
  const x = Math.min(...positions.map((p) => p.x)),
    y = Math.min(...positions.map((p) => p.y)),
    w = Math.max(...positions.map((p) => p.x)) + 210 - x,
    h = Math.max(...positions.map((p) => p.y)) + 90 - y;
  const z = Math.max(
    0.15,
    Math.min(
      1.15,
      ($("#canvas").clientWidth - 80) / w,
      ($("#canvas").clientHeight - 80) / h,
    ),
  );
  state.camera = { x: 40 - x * z, y: 40 - y * z, z };
  transformCanvas();
  saveLayout();
}
function openPicker(title, html) {
  $("#picker-title").textContent = title;
  $("#picker-body").innerHTML = html;
  $("#picker").showModal();
}
async function importList(cursor = null) {
  try {
    const data = await api(
      "/api/import" + (cursor ? "?cursor=" + encodeURIComponent(cursor) : ""),
    );
    openPicker(
      "Import a Codex conversation",
      '<p class="notice">Copies the last 20 turns into a new lead chat.</p>' +
        data.data
          .map(
            (t) =>
              `<button data-import="${esc(t.id)}">${esc(t.name || t.preview || "Untitled conversation")}<small>${esc(t.cwd)}</small></button>`,
          )
          .join("") +
        (data.nextCursor
          ? `<button data-import-page="${esc(data.nextCursor)}">More conversations</button>`
          : ""),
    );
  } catch (e) {
    toast(e.message);
  }
}
async function folders(path) {
  try {
    const data = await api(
      "/api/directories" + (path ? "?path=" + encodeURIComponent(path) : ""),
    );
    openPicker(
      "Project directory",
      `<p class="notice">${esc(data.path)}</p><button class="primary" data-use-directory="${esc(data.path)}">Use this folder</button>${data.parent ? `<button data-directory="${esc(data.parent)}">↑ Parent folder</button>` : ""}` +
        data.directories
          .map(
            (d) =>
              `<button data-directory="${esc(d.path)}">▸ ${esc(d.name)}</button>`,
          )
          .join(""),
    );
  } catch (e) {
    toast(e.message);
  }
}
$("#new-chat").onclick = newChat;
$("#composer").onsubmit = sendMessage;
$("#message").oninput = resizeInput;
$("#message").onkeydown = (e) => {
  if (e.key === "Enter" && !e.shiftKey && !e.isComposing) {
    e.preventDefault();
    $("#composer").requestSubmit();
  }
};
$("#chat-search").oninput = (e) => {
  state.query = e.target.value.toLowerCase();
  render();
};
$("#worker-search").oninput = (e) => {
  state.workerQuery = e.target.value.toLowerCase();
  renderTeam();
};
$("#chat-list").onclick = (e) => {
  const b = e.target.closest("[data-chat]");
  if (b) openConversation(b.dataset.chat);
};
$("#workers").onclick = (e) => {
  const b = e.target.closest("[data-worker]");
  if (b) openConversation(b.dataset.worker);
};
$("#lead-row").onclick = $("#back-lead").onclick = () => {
  if (lead()) openConversation(lead().id);
};
$("#view-toggle").onclick = () =>
  setView(state.view === "chat" ? "canvas" : "chat");
$("#model").onchange = async (e) => {
  if (!current()) return;
  try {
    await api("/api/conversation", { id: state.opened, model: e.target.value });
    await refresh();
  } catch (error) {
    toast(error.message);
    render();
  }
};
$("#stop").onclick = async () => {
  try {
    await api("/api/stop", { id: state.opened, descendants: false });
    await refresh();
  } catch (e) {
    toast(e.message);
  }
};
$("#project").onclick = () => {
  const a = current();
  if (a?.isLead && !a.threadId) folders(a.cwd);
  else if (a?.cwd)
    openPicker("Project directory", `<p class="notice">${esc(a.cwd)}</p>`);
};
$("#sidebar-toggle").onclick = () =>
  document.body.classList.toggle("sidebar-open");
$("#messages").onscroll = () => {
  const e = $("#messages");
  state.autoFollow = e.scrollHeight - e.clientHeight - e.scrollTop < 80;
  $("#jump-latest").hidden = state.autoFollow;
};
$("#jump-latest").onclick = () => {
  state.autoFollow = true;
  $("#messages").scrollTop = $("#messages").scrollHeight;
  $("#jump-latest").hidden = true;
};
$("#messages").onclick = async (e) => {
  const b = e.target.closest("[data-copy]");
  if (b)
    try {
      await navigator.clipboard.writeText(
        state.items.find((i) => i.id === b.dataset.copy)?.text || "",
      );
      toast("Copied.");
    } catch {
      toast("Clipboard access failed.");
    }
};
$("#conversation-menu").onclick = async (e) => {
  const b = e.target.closest("[data-action]");
  if (!b) return;
  $("#conversation-menu").open = false;
  const action = b.dataset.action;
  if (action === "monitor") {
    $("#message").value = "/monitor ";
    $("#message").focus();
    return;
  }
  try {
    if (action === "stop-team")
      await api("/api/stop", { id: current().rootId, descendants: true });
    else await api("/api/action", { id: state.opened, action });
    await refresh();
  } catch (error) {
    toast(error.message);
  }
};
$("#requests").onclick = async (e) => {
  const b = e.target.closest("button");
  if (!b) return;
  if (b.dataset.answer) {
    answerDialog(b.dataset.answer);
    return;
  }
  if (b.dataset.approve)
    try {
      b.disabled = true;
      await api("/api/answer", {
        id: b.dataset.approve,
        decision: b.dataset.decision,
      });
      await refresh();
    } catch (error) {
      toast(error.message);
      b.disabled = false;
    }
};
$("#answer-form").onsubmit = submitAnswer;
$("#close-answer").onclick = () => $("#answer-dialog").close();
$("#monitors").onclick = async (e) => {
  const b = e.target.closest("[data-cancel-watch]");
  if (b)
    try {
      await api("/api/monitor/cancel", { id: b.dataset.cancelWatch });
      await refresh();
    } catch (error) {
      toast(error.message);
    }
};
$("#fit").onclick = fitCanvas;
$("#canvas").onpointerdown = (e) => {
  if (e.target.closest("#fit")) return;
  const node = e.target.closest("[data-node]");
  state.drag = {
    x: e.clientX,
    y: e.clientY,
    id: node?.dataset.node,
    camera: { ...state.camera },
    position: node ? { ...state.positions[node.dataset.node] } : null,
    moved: false,
  };
  $("#canvas").setPointerCapture(e.pointerId);
};
$("#canvas").onpointermove = (e) => {
  const d = state.drag;
  if (!d) return;
  const dx = e.clientX - d.x,
    dy = e.clientY - d.y;
  d.moved ||= Math.abs(dx) + Math.abs(dy) > 4;
  if (d.id) {
    state.positions[d.id] = {
      x: d.position.x + dx / state.camera.z,
      y: d.position.y + dy / state.camera.z,
    };
    const node = [...$("#nodes").children].find((n) => n.dataset.node === d.id);
    node.style.left = state.positions[d.id].x + "px";
    node.style.top = state.positions[d.id].y + "px";
  } else {
    state.camera.x = d.camera.x + dx;
    state.camera.y = d.camera.y + dy;
    transformCanvas();
  }
};
$("#canvas").onpointerup = (e) => {
  const d = state.drag;
  state.drag = null;
  if (!d) return;
  if (!d.moved && d.id) openConversation(d.id);
  else {
    renderCanvas();
    saveLayout();
  }
};
$("#canvas").onpointercancel = () => {
  const d = state.drag;
  state.drag = null;
  if (!d) return;
  state.camera = d.camera;
  if (d.id) state.positions[d.id] = d.position;
  renderCanvas();
};
$("#nodes").onclick = (e) => {
  if (e.detail === 0) {
    const b = e.target.closest("[data-node]");
    if (b) openConversation(b.dataset.node);
  }
};
$("#canvas").addEventListener(
  "wheel",
  (e) => {
    e.preventDefault();
    const rect = $("#canvas").getBoundingClientRect(),
      x = e.clientX - rect.left,
      y = e.clientY - rect.top,
      z = Math.max(
        0.15,
        Math.min(2, state.camera.z * Math.exp(-e.deltaY * 0.002)),
      ),
      ratio = z / state.camera.z;
    state.camera = {
      x: x - (x - state.camera.x) * ratio,
      y: y - (y - state.camera.y) * ratio,
      z,
    };
    transformCanvas();
    saveLayout();
  },
  { passive: false },
);
$("#import-chat").onclick = () => importList();
$("#other-sessions").onclick = () =>
  openPicker(
    "Other sessions",
    state.threads
      .filter((a) => !a.isLead)
      .map(
        (a) =>
          `<button data-open-session="${esc(a.id)}">${esc(a.name)}<small>${esc(a.role || "Agent")} · ${esc(status(a))}</small></button>`,
      )
      .join("") +
      state.chats
        .map(
          (c) =>
            `<button data-open-session="${esc(c.id)}">${esc(c.name)}<small>Shared chat</small></button>`,
        )
        .join("") || '<p class="notice">No other sessions.</p>',
  );
$("#close-picker").onclick = () => $("#picker").close();
$("#picker-body").onclick = async (e) => {
  const b = e.target.closest("button");
  if (!b) return;
  try {
    if (b.dataset.openSession) {
      $("#picker").close();
      openConversation(b.dataset.openSession);
    } else if (b.dataset.directory) await folders(b.dataset.directory);
    else if (b.dataset.useDirectory) {
      await api("/api/conversation", {
        id: state.opened,
        cwd: b.dataset.useDirectory,
      });
      $("#picker").close();
      await refresh();
    } else if (b.dataset.importPage) await importList(b.dataset.importPage);
    else if (b.dataset.import) {
      b.disabled = true;
      const a = await api("/api/import", {
        id: (b.dataset.requestId ||= crypto.randomUUID()),
        threadId: b.dataset.import,
        name: "Imported chat",
        model: $("#model").value,
      });
      $("#picker").close();
      await refresh();
      openConversation(a.id);
    }
  } catch (error) {
    toast(error.message);
    b.disabled = false;
  }
};
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    document.body.classList.remove("sidebar-open");
    $("#conversation-menu").open = false;
  }
  if ((e.metaKey || e.ctrlKey) && e.key === "k") {
    e.preventDefault();
    document.body.classList.add("sidebar-open");
    $("#chat-search").focus();
  }
});
new ResizeObserver(() => {
  if (state.view === "canvas") transformCanvas();
}).observe($("#canvas"));
refresh();

window.addEventListener("pagehide", () => {
  state.opened = null;
  state.closed = true;
  clearTimeout(pollTimer);
  clearTimeout(detailTimer);
});

$("#team-toggle").onclick = () => {
  const open = document.body.classList.toggle("team-open");
  $("#team-toggle").setAttribute("aria-pressed", String(open));
};
$("#team-close").onclick = () => {
  document.body.classList.remove("team-open");
  $("#team-toggle").setAttribute("aria-pressed", "false");
};
