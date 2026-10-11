// The vault site: unlock, browse a state, download a file, manage passkeys (docs/vault.md).
import { Vault } from "./client.js";

const $ = (id) => document.getElementById(id);
const show = (id, on) => ($(id).hidden = !on);
const status = (text) => ($("status").textContent = text ?? "");
const size = (n) =>
  n < 1024
    ? `${n} B`
    : n < 1048576
      ? `${(n / 1024).toFixed(1)} KB`
      : `${(n / 1048576).toFixed(1)} MB`;

let vault;
let slots = [];
let files = {};
let folder = "";

async function guarded(label, work) {
  status(label);
  try {
    await work();
    status("");
  } catch (error) {
    status(`${label.replace(/…$/, "")} failed: ${error.message}`);
  }
}

function lockedView() {
  show("actions", false);
  show("browse", false);
  show("passkey-panel", false);
  show("unlock", true);
  show(
    "with-passkey",
    slots.some((s) => s.name.startsWith("passkey-")),
  );
}

async function openState(seq) {
  files = await vault.reader.files(seq);
  folder = "";
  render();
}

async function unlockedView() {
  show("unlock", false);
  show("actions", true);
  show("browse", true);
  $("paper").value = "";
  const states = vault.reader.states();
  if (!states.length) {
    files = {};
    render();
    status("The VM has not pushed a state yet.");
    return;
  }
  const select = $("state");
  select.replaceChildren(
    ...states
      .reverse()
      .map(
        ({ seq }, index) =>
          new Option(index === 0 ? `#${seq} (latest)` : `#${seq}`, String(seq)),
      ),
  );
  await openState(Number(select.value));
}

function render() {
  const prefix = folder ? `${folder}/` : "";
  const folders = new Map();
  const here = [];
  for (const [path, entry] of Object.entries(files)) {
    if (!path.startsWith(prefix)) continue;
    const rest = path.slice(prefix.length);
    const slash = rest.indexOf("/");
    if (slash >= 0)
      folders.set(
        rest.slice(0, slash),
        (folders.get(rest.slice(0, slash)) ?? 0) + 1,
      );
    else here.push([rest, entry]);
  }
  const crumbs = [
    ["Project", ""],
    ...folder
      .split("/")
      .filter(Boolean)
      .map((part, i, all) => [part, all.slice(0, i + 1).join("/")]),
  ];
  $("crumbs").replaceChildren(
    ...crumbs.map(([name, target]) => {
      const button = document.createElement("button");
      button.textContent = name;
      button.onclick = () => ((folder = target), render());
      return button;
    }),
  );
  const rows = [
    ...[...folders]
      .sort()
      .map(([name, count]) =>
        row(
          name + "/",
          `${count} files`,
          "folder",
          () => ((folder = prefix + name), render()),
        ),
      ),
    ...here
      .sort()
      .map(([name, entry]) =>
        row(name, size(entry[1]), "file", () => download(prefix + name, entry)),
      ),
  ];
  $("entries").replaceChildren(...rows);
}

function row(name, detail, kind, action) {
  const item = document.createElement("li");
  item.className = kind;
  const button = document.createElement("button");
  button.className = "name";
  button.textContent = name;
  button.onclick = action;
  const right = document.createElement("span");
  right.className = "size";
  right.textContent = detail;
  item.append(button, right);
  return item;
}

function download(path, entry) {
  guarded(`Decrypting ${path}…`, async () => {
    const data = await vault.reader.read(entry);
    const url = URL.createObjectURL(new Blob([data]));
    const link = Object.assign(document.createElement("a"), {
      href: url,
      download: path.split("/").pop(),
    });
    link.click();
    setTimeout(() => URL.revokeObjectURL(url), 60_000);
  });
}

async function renderPasskeys() {
  slots = await vault.slots();
  const list = slots.filter((s) => s.name.startsWith("passkey-"));
  $("passkey-list").replaceChildren(
    ...(list.length
      ? list.map((slot) => {
          const item = document.createElement("li");
          item.textContent = slot.name.slice(8, 24) + "…";
          const remove = document.createElement("button");
          remove.textContent = "Remove";
          remove.onclick = () =>
            guarded(
              "Removing the passkey…",
              async () => (
                await vault.removePasskey(slot.name),
                await renderPasskeys()
              ),
            );
          item.append(remove);
          return item;
        })
      : [
          Object.assign(document.createElement("li"), {
            textContent: "No passkey yet. The paper key opens the vault.",
          }),
        ]),
  );
}

$("paper-form").onsubmit = (event) => {
  event.preventDefault();
  guarded(
    "Unlocking…",
    async () => (
      await vault.unlockWithPaper($("paper").value, slots),
      await unlockedView()
    ),
  );
};
$("with-passkey").onclick = () =>
  guarded(
    "Unlocking…",
    async () => (await vault.unlockWithPasskey(slots), await unlockedView()),
  );
$("state").onchange = () =>
  guarded("Reading the state…", () => openState(Number($("state").value)));
$("lock").onclick = () => (vault.lock(), (files = {}), lockedView());
$("passkeys").onclick = () =>
  guarded("Reading the passkeys…", async () => {
    show("browse", false);
    show("passkey-panel", true);
    await renderPasskeys();
  });
$("close-passkeys").onclick = () => (
  show("passkey-panel", false),
  show("browse", true)
);
$("add-form").onsubmit = (event) => {
  event.preventDefault();
  guarded(
    "Adding a passkey…",
    async () => (
      await vault.addPasskey($("label").value),
      await renderPasskeys()
    ),
  );
};

guarded("Loading…", async () => {
  vault = await Vault.open();
  $("project").textContent = vault.prefix;
  document.title = `Vault ${vault.prefix}`;
  slots = await vault.slots();
  lockedView();
});
