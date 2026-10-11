// Desk (docs/vault.md, "Gateway"): a read-only, Finder-like view of the vaults. Column and list
// views, a path bar, Quick Look, downloads, and the passkeys of each vault.
import { Vault, projects } from "./client.js";

const $ = (id) => document.getElementById(id);
const show = (id, on) => ($(id).hidden = !on);
const status = (text) => ($("status").textContent = text ?? "");
const compact = matchMedia("(max-width: 700px)");

const ICON = {
  folder:
    '<svg viewBox="0 0 32 32"><path class="folder-back" d="M3 8.5A2.5 2.5 0 0 1 5.5 6h6.6l2.4 2.5h12A2.5 2.5 0 0 1 29 11v13.5a2.5 2.5 0 0 1-2.5 2.5h-21A2.5 2.5 0 0 1 3 24.5Z"/><path class="folder-front" d="M3 12.5h26v12a2.5 2.5 0 0 1-2.5 2.5h-21A2.5 2.5 0 0 1 3 24.5Z"/></svg>',
  file: '<svg viewBox="0 0 32 32"><path class="doc" d="M8.5 3.5H19l6 6v18a1.5 1.5 0 0 1-1.5 1.5h-15A1.5 1.5 0 0 1 7 27.5v-22.5a1.5 1.5 0 0 1 1.5-1.5Z"/><path class="fold" d="M19 3.5v6h6"/></svg>',
};
const IMAGE = {
  png: "image/png",
  jpg: "image/jpeg",
  jpeg: "image/jpeg",
  gif: "image/gif",
  webp: "image/webp",
  avif: "image/avif",
  svg: "image/svg+xml",
  ico: "image/x-icon",
  bmp: "image/bmp",
};
const MEDIA = {
  mp4: ["video", "video/mp4"],
  m4v: ["video", "video/mp4"],
  mov: ["video", "video/quicktime"],
  webm: ["video", "video/webm"],
  mp3: ["audio", "audio/mpeg"],
  m4a: ["audio", "audio/mp4"],
  wav: ["audio", "audio/wav"],
  ogg: ["audio", "audio/ogg"],
};
const TEXT_LIMIT = 2 * 1024 * 1024;

const app = {
  project: decodeURIComponent(location.pathname.split("/")[1] ?? ""),
  vault: null,
  slots: [],
  root: null,
  dir: [],
  sel: null,
  view: "columns",
  filter: "",
  back: [],
  forward: [],
};
try {
  app.view = localStorage.getItem("desk.view") === "list" ? "list" : "columns";
} catch {}

// ----- the tree of one state -----

function buildTree(files) {
  const root = { name: "", path: "", children: new Map() };
  for (const [path, entry] of Object.entries(files)) {
    const parts = path.split("/");
    let node = root;
    parts.forEach((part, i) => {
      let child = node.children.get(part);
      if (!child) {
        child =
          i === parts.length - 1
            ? { name: part, path, entry }
            : {
                name: part,
                path: parts.slice(0, i + 1).join("/"),
                children: new Map(),
              };
        node.children.set(part, child);
      }
      node = child;
    });
  }
  return root;
}

const isFolder = (node) => Boolean(node?.children);
const byName = (a, b) =>
  a.name.localeCompare(b.name, undefined, {
    numeric: true,
    sensitivity: "base",
  });
const childrenOf = (node) => [...node.children.values()].sort(byName);
function nodeAt(parts) {
  let node = app.root;
  for (const part of parts) node = node?.children?.get(part);
  return node;
}
const selected = () =>
  app.sel === null ? null : nodeAt([...app.dir, app.sel]);
const extension = (name) =>
  name.includes(".") ? name.split(".").pop().toLowerCase() : "";

function size(n) {
  if (n === 0) return "Zero bytes";
  if (n < 1000) return `${n} bytes`;
  const units = ["KB", "MB", "GB"];
  let value = n / 1000;
  let unit = 0;
  while (value >= 1000 && unit < units.length - 1) {
    value /= 1000;
    unit++;
  }
  return `${value < 10 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
}

function kind(node) {
  if (isFolder(node)) return "Folder";
  const [mode] = node.entry;
  if (mode === "120000") return "Alias";
  const ext = extension(node.name);
  if (IMAGE[ext]) return `${ext.toUpperCase()} image`;
  if (MEDIA[ext]) return `${ext.toUpperCase()} ${MEDIA[ext][0]}`;
  if (ext === "pdf") return "PDF document";
  if (mode === "100755") return "Executable";
  return ext ? `${ext.toUpperCase()} file` : "Document";
}

function folderSize(node) {
  const count = node.children.size;
  return count === 1 ? "1 item" : `${count} items`;
}

// ----- navigation -----

function go(dir, sel, { record = true } = {}) {
  const changed = dir.join("/") !== app.dir.join("/");
  if (record && changed) {
    app.back.push({ dir: app.dir, sel: app.sel });
    app.forward = [];
  }
  app.dir = dir;
  app.sel = sel;
  render();
}

function open(node) {
  if (!node) return;
  if (isFolder(node)) go(node.path.split("/"), null);
  else quickLook();
}

function parent() {
  if (!app.dir.length) return;
  go(app.dir.slice(0, -1), app.dir[app.dir.length - 1]);
}

function step(delta) {
  const list = childrenOf(nodeAt(app.dir) ?? app.root);
  if (!list.length) return;
  const at = list.findIndex((n) => n.name === app.sel);
  const next = Math.max(0, Math.min(list.length - 1, at + delta));
  go(app.dir, list[at < 0 ? 0 : next].name, { record: false });
  if (!$("quicklook-panel").hidden) quickLook();
}

// ----- rendering -----

function item(node, { active, where } = {}) {
  const li = document.createElement("li");
  li.className = "item";
  li.setAttribute("role", "option");
  li.dataset.kind = isFolder(node) ? "folder" : "file";
  if (node.entry?.[0] === "120000") li.dataset.link = "";
  const icon = document.createElement("span");
  icon.className = "icon";
  icon.innerHTML = isFolder(node) ? ICON.folder : ICON.file;
  const ext = extension(node.name);
  if (!isFolder(node) && ext && ext.length <= 4) {
    const badge = document.createElement("span");
    badge.className = "badge";
    badge.textContent = ext;
    icon.append(badge);
  }
  const name = document.createElement("span");
  name.className = "name";
  name.textContent = where ? node.path : node.name;
  li.append(icon, name);
  if (active !== undefined) li.setAttribute("aria-selected", String(active));
  return li;
}

function columnOf(folder, dirParts, selName, focused) {
  const column = document.createElement("ul");
  column.className = "column";
  column.setAttribute("role", "listbox");
  for (const node of childrenOf(folder)) {
    const li = item(node, { active: node.name === selName });
    if (node.name === selName) li.classList.add(focused ? "focus" : "trail");
    if (isFolder(node)) {
      const chevron = document.createElement("span");
      chevron.className = "chevron";
      chevron.textContent = "›";
      li.append(chevron);
    }
    li.onclick = () => go(dirParts, node.name);
    li.ondblclick = () => (isFolder(node) ? null : quickLook());
    column.append(li);
  }
  if (!folder.children.size) {
    const empty = document.createElement("li");
    empty.className = "nothing";
    empty.textContent = "Empty folder";
    column.append(empty);
  }
  return column;
}

function previewColumn(node) {
  const box = document.createElement("div");
  box.className = "column preview";
  const icon = document.createElement("div");
  icon.className = "big-icon";
  icon.innerHTML = ICON.file;
  const name = document.createElement("h3");
  name.textContent = node.name;
  const facts = document.createElement("dl");
  for (const [term, value] of [
    ["Kind", kind(node)],
    ["Size", size(node.entry[1])],
    ["SHA-256", `${node.entry[2].slice(0, 16)}…`],
  ]) {
    const dt = document.createElement("dt");
    dt.textContent = term;
    const dd = document.createElement("dd");
    dd.textContent = value;
    facts.append(dt, dd);
  }
  const actions = document.createElement("div");
  actions.className = "row";
  const look = Object.assign(document.createElement("button"), {
    type: "button",
    textContent: "Quick Look",
    onclick: () => quickLook(),
  });
  const get = Object.assign(document.createElement("button"), {
    type: "button",
    textContent: "Download",
    onclick: () => download(node),
  });
  actions.append(look, get);
  box.append(icon, name, facts, actions);
  return box;
}

function renderColumns() {
  const holder = $("columns");
  const columns = [];
  for (let depth = 0; depth <= app.dir.length; depth++) {
    const dirParts = app.dir.slice(0, depth);
    const selName = depth < app.dir.length ? app.dir[depth] : app.sel;
    columns.push(
      columnOf(nodeAt(dirParts), dirParts, selName, depth === app.dir.length),
    );
  }
  const node = selected();
  if (isFolder(node))
    columns.push(columnOf(node, [...app.dir, app.sel], null, false));
  else if (node) columns.push(previewColumn(node));
  holder.replaceChildren(...columns);
  holder.scrollLeft = holder.scrollWidth;
}

function renderList() {
  const rows = $("list-rows");
  const filter = app.filter.toLowerCase();
  let nodes;
  if (filter) {
    nodes = [];
    const walk = (folder) => {
      for (const node of childrenOf(folder)) {
        if (node.name.toLowerCase().includes(filter)) nodes.push(node);
        if (isFolder(node)) walk(node);
      }
    };
    walk(app.root);
    nodes = nodes.slice(0, 500);
  } else nodes = childrenOf(nodeAt(app.dir) ?? app.root);
  rows.replaceChildren(
    ...nodes.map((node) => {
      const li = item(node, {
        active: !filter && node.name === app.sel,
        where: Boolean(filter),
      });
      if (!filter && node.name === app.sel) li.classList.add("focus");
      const sizeCell = document.createElement("span");
      sizeCell.className = "size";
      sizeCell.textContent = isFolder(node)
        ? folderSize(node)
        : size(node.entry[1]);
      const kindCell = document.createElement("span");
      kindCell.className = "kind";
      kindCell.textContent = kind(node);
      li.append(sizeCell, kindCell);
      const parts = node.path.split("/");
      const select = () => {
        if (filter) {
          app.filter = "";
          $("filter").value = "";
        }
        go(parts.slice(0, -1), parts[parts.length - 1]);
      };
      if (compact.matches) li.onclick = () => (select(), open(node));
      else {
        li.onclick = select;
        li.ondblclick = () => open(node);
      }
      return li;
    }),
  );
  if (!nodes.length) {
    const empty = document.createElement("li");
    empty.className = "nothing";
    empty.textContent = filter ? "No matches" : "Empty folder";
    rows.append(empty);
  }
}

function renderPath() {
  const crumbs = [
    [app.project, []],
    ...app.dir.map((part, i) => [part, app.dir.slice(0, i + 1)]),
  ];
  if (app.sel !== null) crumbs.push([app.sel, null]);
  $("crumbs").replaceChildren(
    ...crumbs.map(([name, target], i) => {
      const button = document.createElement("button");
      button.type = "button";
      button.textContent = name;
      if (target)
        button.onclick = () =>
          go(target.slice(0, -1), target.length ? target.at(-1) : null);
      else button.disabled = true;
      if (i === 0) button.onclick = () => go([], null);
      return button;
    }),
  );
}

function render() {
  if (!app.root) return;
  const list = app.view === "list" || compact.matches || Boolean(app.filter);
  show("columns", !list);
  show("list", list);
  if (list) renderList();
  else renderColumns();
  renderPath();
  $("title").textContent = app.dir.length ? app.dir.at(-1) : app.project;
  $("back").disabled = !app.back.length && !(compact.matches && app.dir.length);
  $("forward").disabled = !app.forward.length;
  $("quicklook").disabled = !selected() || isFolder(selected());
  $("view-columns").setAttribute(
    "aria-pressed",
    String(app.view === "columns"),
  );
  $("view-list").setAttribute("aria-pressed", String(app.view === "list"));
  const folder = nodeAt(app.dir);
  status(folder ? folderSize(folder) : "");
}

// ----- files -----

async function contentOf(node) {
  return app.vault.reader.read(node.entry);
}

function save(name, data) {
  const url = URL.createObjectURL(new Blob([data]));
  const link = Object.assign(document.createElement("a"), {
    href: url,
    download: name,
  });
  link.click();
  setTimeout(() => URL.revokeObjectURL(url), 60_000);
}

function download(node) {
  guarded(`Decrypting ${node.name}…`, async () =>
    save(node.name, await contentOf(node)),
  );
}

let previewUrl;
async function quickLook() {
  const node = selected();
  if (!node || isFolder(node)) return;
  show("quicklook-panel", true);
  $("ql-name").textContent = node.name;
  $("ql-download").onclick = () => download(node);
  const body = $("ql-body");
  body.replaceChildren(
    Object.assign(document.createElement("p"), {
      className: "note",
      textContent: "Decrypting…",
    }),
  );
  if (previewUrl) URL.revokeObjectURL(previewUrl);
  previewUrl = null;
  try {
    const data = await contentOf(node);
    if (selected() !== node) return;
    const ext = extension(node.name);
    let view;
    if (node.entry[0] === "120000") {
      view = document.createElement("p");
      view.className = "note";
      view.textContent = `Alias to ${new TextDecoder().decode(data)}`;
    } else if (IMAGE[ext] || MEDIA[ext] || ext === "pdf") {
      const type = IMAGE[ext] ?? MEDIA[ext]?.[1] ?? "application/pdf";
      previewUrl = URL.createObjectURL(new Blob([data], { type }));
      if (IMAGE[ext])
        view = Object.assign(new Image(), { src: previewUrl, alt: node.name });
      else if (MEDIA[ext])
        view = Object.assign(document.createElement(MEDIA[ext][0]), {
          src: previewUrl,
          controls: true,
        });
      else
        view = Object.assign(document.createElement("iframe"), {
          src: previewUrl,
          title: node.name,
        });
    } else {
      const text = textOf(data);
      if (text === null) {
        view = document.createElement("div");
        view.className = "no-preview";
        view.innerHTML = ICON.file;
        const facts = document.createElement("p");
        facts.textContent = `${kind(node)}, ${size(node.entry[1])}`;
        view.append(facts);
      } else {
        view = document.createElement("pre");
        view.textContent =
          text.length > TEXT_LIMIT
            ? `${text.slice(0, TEXT_LIMIT)}\n\n… (the rest is in the download)`
            : text;
      }
    }
    body.replaceChildren(view);
  } catch (error) {
    body.replaceChildren(
      Object.assign(document.createElement("p"), {
        className: "note",
        textContent: `Decrypting failed: ${error.message}`,
      }),
    );
  }
}

function textOf(data) {
  const head = data.subarray(0, 8192);
  if (head.includes(0)) return null;
  try {
    return new TextDecoder("utf-8", { fatal: true }).decode(data);
  } catch {
    return null;
  }
}

function closeQuickLook() {
  show("quicklook-panel", false);
  $("ql-body").replaceChildren();
  if (previewUrl) URL.revokeObjectURL(previewUrl);
  previewUrl = null;
}

// ----- lock, unlock, passkeys -----

async function guarded(label, work) {
  status(label);
  try {
    await work();
    status(statusText());
  } catch (error) {
    status(`${label.replace(/…$/, "")} failed: ${error.message}`);
  }
}
const statusText = () => {
  const folder = app.root && nodeAt(app.dir);
  return folder ? folderSize(folder) : "";
};

function lockedView() {
  app.root = null;
  show("columns", false);
  show("list", false);
  show("browse-tools", false);
  show("sidebar-foot", false);
  show("unlock", true);
  $("unlock-name").textContent = app.project;
  $("title").textContent = app.project;
  $("crumbs").replaceChildren();
  show(
    "with-passkey",
    app.slots.some((s) => s.name.startsWith("passkey-")),
  );
  $("paper-box").open = !app.slots.some((s) => s.name.startsWith("passkey-"));
}

async function unlockedView() {
  $("paper").value = "";
  show("unlock", false);
  show("browse-tools", true);
  show("sidebar-foot", true);
  const states = app.vault.reader.states();
  if (!states.length) {
    show("empty", true);
    $("empty-text").textContent = "The VM has not pushed a state yet.";
    return;
  }
  $("state").replaceChildren(
    ...[...states]
      .reverse()
      .map(
        ({ seq }, i) =>
          new Option(i === 0 ? `Latest (#${seq})` : `#${seq}`, String(seq)),
      ),
  );
  await openState(states.at(-1).seq);
}

async function openState(seq) {
  app.root = buildTree(await app.vault.reader.files(seq));
  // Keep the place when it still exists in the other version.
  while (app.dir.length && !isFolder(nodeAt(app.dir))) app.dir.pop();
  if (app.sel !== null && !selected()) app.sel = null;
  render();
}

async function renderPasskeys() {
  app.slots = await app.vault.slots();
  const passkeys = app.slots.filter((s) => s.name.startsWith("passkey-"));
  $("passkey-list").replaceChildren(
    ...(passkeys.length
      ? passkeys.map((slot) => {
          const li = document.createElement("li");
          const name = document.createElement("span");
          name.textContent = `${slot.name.slice(8, 24)}…`;
          const remove = Object.assign(document.createElement("button"), {
            type: "button",
            textContent: "Remove",
          });
          remove.onclick = () =>
            guarded("Removing the passkey…", async () => {
              await app.vault.removePasskey(slot.name);
              await renderPasskeys();
            });
          li.append(name, remove);
          return li;
        })
      : [
          Object.assign(document.createElement("li"), {
            className: "note",
            textContent: "No passkey yet. The paper key opens the vault.",
          }),
        ]),
  );
}

// ----- events -----

$("paper-form").onsubmit = (event) => {
  event.preventDefault();
  guarded("Unlocking…", async () => {
    await app.vault.unlockWithPaper($("paper").value, app.slots);
    await unlockedView();
  });
};
$("with-passkey").onclick = () =>
  guarded("Unlocking…", async () => {
    await app.vault.unlockWithPasskey(app.slots);
    await unlockedView();
  });
$("state").onchange = () =>
  guarded("Reading the version…", () => openState(Number($("state").value)));
$("lock").onclick = () => {
  closeQuickLook();
  app.vault.lock();
  lockedView();
  status("");
};
$("view-columns").onclick = () => setView("columns");
$("view-list").onclick = () => setView("list");
function setView(view) {
  app.view = view;
  try {
    localStorage.setItem("desk.view", view);
  } catch {}
  render();
}
$("back").onclick = () => {
  if (compact.matches && !app.back.length) return parent();
  const to = app.back.pop();
  if (!to) return;
  app.forward.push({ dir: app.dir, sel: app.sel });
  go(to.dir, to.sel, { record: false });
};
$("forward").onclick = () => {
  const to = app.forward.pop();
  if (!to) return;
  app.back.push({ dir: app.dir, sel: app.sel });
  go(to.dir, to.sel, { record: false });
};
$("quicklook").onclick = () => quickLook();
$("ql-close").onclick = closeQuickLook;
$("quicklook-panel").onclick = (event) =>
  event.target === $("quicklook-panel") && closeQuickLook();
$("filter").oninput = () => ((app.filter = $("filter").value.trim()), render());
$("passkeys").onclick = () =>
  guarded("Reading the passkeys…", async () => {
    $("passkey-vault").textContent = app.project;
    show("passkey-panel", true);
    await renderPasskeys();
  });
$("close-passkeys").onclick = () => show("passkey-panel", false);
$("add-form").onsubmit = (event) => {
  event.preventDefault();
  guarded("Adding a passkey…", async () => {
    await app.vault.addPasskey($("label").value.trim());
    $("label").value = "";
    await renderPasskeys();
  });
};
$("use-existing").onclick = () =>
  guarded("Adding the passkey…", async () => {
    await app.vault.useExistingPasskey();
    await renderPasskeys();
  });
const toggleSidebar = (on) => {
  document.body.classList.toggle("sidebar-open", on);
  show("sidebar-scrim", on);
};
$("toggle-sidebar").onclick = () =>
  toggleSidebar(!document.body.classList.contains("sidebar-open"));
$("sidebar-scrim").onclick = () => toggleSidebar(false);
compact.onchange = () => render();

document.addEventListener("keydown", (event) => {
  const typing = event.target.closest?.("input, textarea, select");
  const quick = !$("quicklook-panel").hidden;
  if (event.key === "Escape") {
    if (quick) return closeQuickLook();
    if (!$("passkey-panel").hidden) return show("passkey-panel", false);
  }
  if (typing || !app.root || !$("passkey-panel").hidden) return;
  const node = selected();
  const keys = {
    ArrowDown: () => step(1),
    ArrowUp: () => step(-1),
    ArrowRight: () => {
      if (app.view !== "columns" || !isFolder(node)) return;
      const first = childrenOf(node)[0];
      go([...app.dir, app.sel], first ? first.name : null);
    },
    ArrowLeft: () => app.view === "columns" && parent(),
    " ": () => (quick ? closeQuickLook() : quickLook()),
    Enter: () => open(node),
  };
  if (event.metaKey && event.key === "ArrowUp") {
    event.preventDefault();
    return parent();
  }
  if (event.metaKey && event.key === "ArrowDown") {
    event.preventDefault();
    return open(node);
  }
  if (keys[event.key] && !event.metaKey && !event.ctrlKey) {
    event.preventDefault();
    keys[event.key]();
  }
});

// ----- start -----

guarded("Loading…", async () => {
  const names = await projects();
  $("vaults").replaceChildren(
    ...names.map((name) => {
      const li = document.createElement("li");
      const link = Object.assign(document.createElement("a"), {
        href: `/${encodeURIComponent(name)}/`,
        textContent: name,
      });
      link.prepend(
        Object.assign(document.createElement("span"), {
          className: "icon",
          innerHTML: ICON.folder,
        }),
      );
      if (name === app.project) link.setAttribute("aria-current", "page");
      li.append(link);
      return li;
    }),
  );
  if (!app.project || !names.includes(app.project)) {
    if (names.length === 1)
      return location.replace(`/${encodeURIComponent(names[0])}/`);
    show("empty", true);
    $("empty-text").textContent = app.project
      ? `There is no vault ${app.project}.`
      : names.length
        ? "Choose a vault."
        : "This desk has no vaults yet.";
    return;
  }
  document.title = `${app.project} · Desk`;
  app.vault = await Vault.open(app.project);
  app.slots = await app.vault.slots();
  lockedView();
});
