// These check the fixture's verdict logic. Real hidden Tauri execution is required.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import vm from "node:vm";

const script = await readFile(
  new URL("./src-tauri/src/attachment-event-privacy.js", import.meta.url),
  "utf8",
);
const frameScript = await readFile(
  new URL("./src-tauri/src/attachment-frame-privacy.js", import.meta.url),
  "utf8",
);

async function exercise({
  allowListen,
  rustDelivered = 4,
  foreignLeaked = false,
  foreignIPC = false,
  unsafeAccepted = true,
  brokerAvailable = true,
}) {
  const config = {
    begin: "fixture-begin",
    finish: "fixture-finish",
    events: ["raw-enter", "raw-drop"],
    emissions: 4,
    markerPrefix: "fixture-owned-marker-",
    listenerObject: "fixtureRegistry",
    emitterFunction: "fixtureEmitter",
    forgedListenerId: 4000,
    failures: {
      raw: "raw_native_path",
      admission: "listener_admission",
      setup: "fixture_setup",
      foreign: "foreign_frame",
    },
    foreign: {
      origin: "studio-view://studio-attachment-privacy.localhost",
      documentPath: "/assets/native-attachment-privacy-frame.html",
      namePrefix: "attachment-foreign-",
      resultKind: "fixture-foreign-result",
    },
    broker: {
      connect: "ui.events.connect",
      disconnect: "ui.events.disconnect",
    },
  };
  const callbacks = new Map(),
    registrations = [];
  let callbackId = 0,
    result;
  const internal = {
    callbacks,
    transformCallback(callback) {
      callbacks.set(++callbackId, callback);
      return callbackId;
    },
  };
  Object.defineProperty(internal, "runCallback", {
    value: (id, value) => callbacks.get(id)?.(value),
  });
  const window = {
    __TAURI_INTERNALS__: internal,
    __TAURI_EVENT_PLUGIN_INTERNALS__: { unregisterListener() {} },
  };
  const listeners = new Set();
  window.addEventListener = (_name, callback) => listeners.add(callback);
  window.removeEventListener = (_name, callback) => listeners.delete(callback);
  const document = {
    createElement() {
      return {
        contentWindow: {},
        setAttribute(name, value) {
          this[name] = value;
        },
        remove() {},
      };
    },
    body: {
      append(frame) {
        const control = frame.name.endsWith("unsafe-control");
        const origin = frame.src
          ? config.foreign.origin
          : frame.sandbox
            ? "null"
            : "tauri://localhost";
        queueMicrotask(() => {
          for (const callback of listeners)
            callback({
              source: frame.contentWindow,
              origin,
              data: {
                kind: config.foreign.resultKind,
                id: frame.name,
                ownIPC: !control && foreignIPC,
                parentIPC: control,
                unsafeAccepted: control && unsafeAccepted,
              },
            });
        });
      },
    },
  };
  Object.defineProperty(window, config.emitterFunction, { value() {} });
  window.__TAURI__ = {
    core: {
      Channel: class {},
      async invoke(command, args) {
        if (command === "plugin:event|listen") {
          if (!allowListen) throw Error("Denied fixture registration");
          registrations.push(args);
          return registrations.length;
        }
        assert.equal(command, "client_request");
        const request = args.request;
        if (request.method === config.broker.connect) {
          if (
            !brokerAvailable ||
            Object.keys(request.params).some((key) => key !== "callback")
          )
            throw Error("Fixture broker refused");
          return { id: request.id, result: { leaseId: "fixture-ui-lease" } };
        }
        if (request.method === config.broker.disconnect) {
          assert.deepEqual(Object.keys(request.params), ["leaseId"]);
          assert.equal(request.params.leaseId, "fixture-ui-lease");
          return { id: request.id, result: { released: true } };
        }
        if (request.method === config.begin) {
          // The fixture receives a native packet only in this deliberate bad case.
          // This is a harness control, not a replacement for Tauri's Rust gate.
          for (const registration of registrations)
            callbacks.get(registration.handler)?.({
              event: registration.event,
              payload: { paths: [`/${config.markerPrefix}private`] },
            });
          return {
            id: request.id,
            result: { rustDelivered, foreignAdmissions: foreignLeaked ? 2 : 1 },
          };
        }
        assert.equal(request.method, config.finish);
        result = request.params;
        return { id: request.id, result: { recorded: true } };
      },
    },
  };
  await vm.runInNewContext(script, {
    window,
    config,
    document,
    location: { origin: "tauri://localhost" },
  });
  assert.ok(result, "the fixture must record a terminal verdict");
  return result;
}

test("privacy fixture rejects an admitted native raw-path packet", async () => {
  const result = await exercise({ allowListen: true });
  assert.equal(result.passed, false);
  assert.equal(result.allowed, 8);
  assert.ok(result.rawLeakCount > 0);
  assert.equal(result.failure, "raw_native_path");
});

test("privacy fixture requires denied registrations and positive Rust delivery", async () => {
  const result = await exercise({ allowListen: false });
  assert.equal(result.passed, true);
  assert.equal(result.denied, 8);
  assert.equal(result.rawLeakCount, 0);
  assert.equal(result.fakeRegistry, 2);
  assert.equal(result.callbackIntercept, true);
  assert.equal(result.registerApiAbsent, true);
});

test("privacy fixture cannot pass without its positive Rust delivery control", async () => {
  const result = await exercise({ allowListen: false, rustDelivered: 0 });
  assert.equal(result.passed, false);
  assert.equal(result.rustDelivered, 0);
});

test("privacy fixture rejects a foreign native command admission", async () => {
  const result = await exercise({ allowListen: false, foreignLeaked: true });
  assert.equal(result.passed, false);
});

test("privacy fixture rejects an inherited foreign IPC API", async () => {
  const result = await exercise({ allowListen: false, foreignIPC: true });
  assert.equal(result.passed, false);
  assert.equal(result.foreignDenied, 0);
});

test("privacy fixture requires its unsafe same-origin positive control", async () => {
  const result = await exercise({ allowListen: false, unsafeAccepted: false });
  assert.equal(result.passed, false);
  assert.equal(result.unsafeControl, false);
});

test("privacy fixture cannot pass with an unregistered event broker", async () => {
  const result = await exercise({ allowListen: false, brokerAvailable: false });
  assert.equal(result.passed, false);
  assert.equal(result.brokerReady, false);
  assert.equal(result.failure, "fixture_setup");
});

async function exerciseFrame(parentReadable) {
  const packets = [];
  let report;
  const config = {
    namePrefix: "attachment-foreign-",
    documentPath: "/fixture.html",
    attempt: "fixture-attempt",
    resultKind: "fixture-frame-result",
  };
  const parent = {
    postMessage(value) {
      if (value.kind === config.resultKind) report = value;
    },
  };
  Object.defineProperty(parent, "__TAURI__", {
    get() {
      if (!parentReadable) throw Error("Fixture cross-origin access denied");
      return {
        core: {
          async invoke(command, { request }) {
            assert.equal(command, "client_request");
            assert.equal(request.method, config.attempt);
            return { id: request.id, result: { accepted: true } };
          },
        },
      };
    },
  });
  const window = {
    parent,
    name: `${config.namePrefix}unit`,
    webkit: {
      messageHandlers: {
        ipc: {
          postMessage(value) {
            packets.push(JSON.parse(value));
          },
        },
      },
    },
  };
  vm.runInNewContext(frameScript, {
    window,
    parent,
    location: { href: "about:srcdoc" },
    config,
  });
  await Promise.resolve();
  assert.ok(report);
  return { report, packets };
}

test("foreign probe never submits a guessed key or obtains a parent API", async () => {
  const { report, packets } = await exerciseFrame(false);
  assert.equal(report.parentIPC, false);
  assert.equal(report.ownIPC, false);
  assert.equal(report.unsafeAccepted, false);
  assert.equal(report.keylessPacketSent, true);
  assert.equal(packets.length, 1);
  assert.equal(Object.hasOwn(packets[0], "__TAURI_INVOKE_KEY__"), false);
});

test("foreign probe detects the unsafe same-origin parent API control", async () => {
  const { report } = await exerciseFrame(true);
  assert.equal(report.parentIPC, true);
  assert.equal(report.unsafeAccepted, true);
});
