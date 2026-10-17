(async () => {
  if (window.__studioCheckStarted) return;
  window.__studioCheckStarted = true;
  const call = async (id, method, params = null) => {
    await window.__TAURI__.event.emit("studio-native-check-step", {
      id,
      method,
      origin: location.origin,
    });
    const response = await window.__TAURI__.core.invoke("client_request", {
      request: { id, method, params },
    });
    if (response.id !== id) throw new Error("Response identity changed");
    return response;
  };
  const assert = (condition, message) => {
    if (!condition) throw new Error(message);
  };
  try {
    if (window.__studioCheckMode === "profile-census") {
      const deadline = Date.now() + 540000;
      while (
        Date.now() < deadline &&
        typeof window.localStorage.flush !== "function"
      ) {
        const message = document.getElementById("root")?.textContent || "";
        if (message.includes("could not recover")) throw new Error(message);
        await new Promise((resolve) => setTimeout(resolve, 100));
      }
      assert(
        typeof window.localStorage.flush === "function",
        "Large profile bootstrap did not finish",
      );
      const response = await call("census-manifest", "profile.snapshot", {
        owner: "root",
      });
      assert(
        response.result?.snapshot?.version === 2,
        "Expected chunked profile",
      );
      let records = 0;
      for (const saved of response.result.snapshot.databases) {
        const db = await new Promise((resolve, reject) => {
          const request = indexedDB.open(saved.name, saved.version);
          request.onupgradeneeded = () => request.transaction.abort();
          request.onsuccess = () => resolve(request.result);
          request.onerror = () => reject(request.error);
        });
        try {
          assert(
            db.objectStoreNames.length === saved.stores.length,
            "Imported schema count changed",
          );
          for (const store of saved.stores) {
            const count = await new Promise((resolve, reject) => {
              const request = db
                .transaction(store.name)
                .objectStore(store.name)
                .count();
              request.onsuccess = () => resolve(request.result);
              request.onerror = () => reject(request.error);
            });
            const expected = store.chunks.reduce(
              (sum, chunk) => sum + chunk.records,
              0,
            );
            assert(
              count === expected,
              "Imported database record count changed",
            );
            records += count;
          }
        } finally {
          db.close();
        }
      }
      await window.localStorage.flush();
      await window.__TAURI__.event.emit("studio-native-check-step", {
        kind: "profile-census",
        databases: response.result.snapshot.databases.length,
        records,
      });
      await window.__TAURI__.event.emit("studio-native-check-result", {
        passed: true,
        mode: window.__studioCheckMode,
      });
      return;
    }
    if (window.__studioCheckMode.startsWith("profile-store-")) {
      const db = await new Promise((resolve, reject) => {
        const request = indexedDB.open("native-profile-isolation", 1);
        request.onupgradeneeded = () =>
          request.result.createObjectStore("records");
        request.onsuccess = () => resolve(request.result);
        request.onerror = () => reject(request.error);
      });
      const write = window.__studioCheckMode === "profile-store-write";
      const value = await new Promise((resolve, reject) => {
        const tx = db.transaction("records", write ? "readwrite" : "readonly");
        const store = tx.objectStore("records");
        const request = write
          ? store.put(new Blob([new Uint8Array([0, 255, 42])]), "recording")
          : store.get("recording");
        tx.oncomplete = () => resolve(request.result);
        tx.onabort = () =>
          reject(tx.error || new Error("Profile transaction aborted"));
        tx.onerror = () => reject(tx.error);
      });
      if (!write) {
        if (window.__studioCheckMode === "profile-store-empty")
          assert(
            value === undefined,
            "Another profile's recording leaked into a fresh profile",
          );
        else {
          assert(
            value instanceof Blob,
            "Recording did not survive profile restart",
          );
          assert(
            JSON.stringify([...new Uint8Array(await value.arrayBuffer())]) ===
              "[0,255,42]",
            "Recording bytes changed after restart",
          );
        }
      }
      db.close();
      await window.__TAURI__.event.emit("studio-native-check-result", {
        passed: true,
        mode: window.__studioCheckMode,
      });
      return;
    }
    if (window.__studioCheckMode === "network-shell") {
      const deadline = Date.now() + 15000;
      while (
        !document.querySelector('[data-navigation="combined"]') &&
        Date.now() < deadline
      )
        await new Promise((resolve) => setTimeout(resolve, 50));
      assert(
        document.querySelector('[data-navigation="combined"]'),
        "Combined shell did not mount",
      );
      assert(
        document.querySelectorAll("iframe").length === 0,
        "Combined shell created a server iframe",
      );
      const response = await call("shell-http-command", "api.request", {
        path: "/api/native-check",
        method: "POST",
        body: null,
        requestId: "shell-http-command",
        httpResponse: true,
      });
      const result = response.result;
      assert(
        !response.error &&
          result?.status === 409 &&
          JSON.stringify(
            Array.from(atob(result.base64), (byte) => byte.charCodeAt(0)),
          ) === "[97,0,255]",
        "Direct shell HTTP round trip failed: " + JSON.stringify(response),
      );
      await window.__TAURI__.event.emit("studio-native-check-result", {
        passed: true,
        mode: window.__studioCheckMode,
      });
      return;
    }
    if (window.__studioCheckMode === "write") {
      const notification = await call("hidden-notification", "desktop.notify", {
        title: "Fixture",
        body: "Fixture",
        target: { serverId: "remote", agentId: "chat", section: "messages" },
      });
      assert(
        notification.result === false,
        "Hidden checks must not show a notification",
      );
      const invalid = await call("invalid-notification", "desktop.notify", {
        title: "Fixture",
        body: "Fixture",
        target: { agentId: "chat", section: "files" },
      });
      assert(!!invalid.error, "Invalid notification target was accepted");
    }
    if (window.__studioCheckMode === "profile-import") {
      await window.__TAURI__.event.emit("studio-native-check-step", {
        kind: "profile-bootstrap",
        secure: isSecureContext,
        locks: !!navigator.locks,
        indexedDB: !!window.indexedDB,
        root: document.getElementById("root")?.textContent,
      });
      const deadline = Date.now() + 5000;
      while (
        Date.now() < deadline &&
        typeof window.localStorage.flush !== "function"
      ) {
        const message = document.getElementById("root")?.textContent || "";
        if (message.includes("could not recover")) throw new Error(message);
        await new Promise((resolve) => setTimeout(resolve, 50));
      }
      assert(
        typeof window.localStorage.flush === "function",
        "Profile bootstrap did not finish",
      );
      assert(
        window.localStorage.getItem("migrated") === "saved Ж🦀",
        "Imported preference was lost",
      );
      assert(
        window.localStorage.getItem(":server:remote:draft") ===
          "remote draft survives",
        "Remote owner preference was lost",
      );
      const remoteDb = await new Promise((resolve, reject) => {
        const request = indexedDB.open("migration-fixture:server:remote", 1);
        request.onsuccess = () => resolve(request.result);
        request.onerror = () => reject(request.error);
      });
      const remoteValue = await new Promise((resolve, reject) => {
        const request = remoteDb
          .transaction("records")
          .objectStore("records")
          .get("voice");
        request.onsuccess = () => resolve(request.result);
        request.onerror = () => reject(request.error);
      });
      remoteDb.close();
      assert(remoteValue === "remote bytes", "Remote owner database was lost");
      await window.localStorage.flush();
      const stored = await call(
        "profile-preferences",
        "preferences.raw_all",
        {},
      );
      assert(
        stored.result?.['browser:"root":migrated'] === "saved Ж🦀",
        "Imported preference did not reach Rust",
      );
      assert(
        !Object.hasOwn(
          stored.result,
          'browser:"root":studio-browser-profile-import-v1',
        ),
        "Import receipt leaked into settings",
      );
      const db = await new Promise((resolve, reject) => {
        const request = indexedDB.open("migration-fixture", 1);
        request.onsuccess = () => resolve(request.result);
        request.onerror = () => reject(request.error);
      });
      const record = await new Promise((resolve, reject) => {
        const request = db
          .transaction("records")
          .objectStore("records")
          .get("voice");
        request.onsuccess = () => resolve(request.result);
        request.onerror = () => reject(request.error);
      });
      const nextKey = await new Promise((resolve, reject) => {
        const transaction = db.transaction("changes", "readwrite");
        let key;
        const request = transaction
          .objectStore("changes")
          .add({ text: "next" });
        request.onsuccess = () => {
          key = request.result;
        };
        transaction.oncomplete = () => resolve(key);
        transaction.onabort = () => reject(transaction.error);
      });
      assert(nextKey === 101, "Imported key generator changed");
      db.close();
      assert(
        record.pcm instanceof Int16Array &&
          record.pcm[0] === -32768 &&
          record.pcm[2] === 32767,
        "Imported PCM changed",
      );
      assert(
        record.file instanceof File && record.file.name === "bytes.bin",
        "Imported file metadata changed",
      );
      assert(
        new Uint8Array(await record.file.arrayBuffer())[1] === 255,
        "Imported file bytes changed",
      );
      await window.__TAURI__.event.emit("studio-native-check-result", {
        passed: true,
        mode: "profile-import",
      });
      return;
    }
    for (const method of [
      "desktop.fileAction",
      "desktop.revealPath",
      "desktop.showVaultKeys",
      "desktop.saveBegin",
      "desktop.requestMicrophone",
      "desktop.prepareTranscription",
      "desktop.cancelTranscription",
      "desktop.openExternal",
      "desktop.openSignIn",
      "desktop.pickDirectory",
      "desktop.pickAttachments",
    ]) {
      let actionDenied = false;
      try {
        await call(`forged-${method}`, method, {
          permit: 123,
          value: {
            action: "open",
            target: {
              agent: "test",
              path: "/Users/igor/Rescue/projects-recovery-20261015/studio-session-recovery/b3e26411-source-recovery/source-scripts/test.txt",
            },
          },
        });
      } catch (error) {
        actionDenied = String(error).includes(
          "Use a desktop button or keyboard action.",
        );
      }
      assert(
        actionDenied,
        "A synthetic request acquired native file privileges",
      );
    }
    if (window.__studioCheckMode === "frames") {
      const frame = document.createElement("iframe");
      const response = new Promise((resolve) => {
        const listener = (event) => {
          if (
            event.source !== frame.contentWindow ||
            event.data?.kind !== "native-frame-probe"
          )
            return;
          window.removeEventListener("message", listener);
          resolve({ ...event.data, messageOrigin: event.origin });
        };
        window.addEventListener("message", listener);
      });
      frame.src =
        "studio-view://studio-nrxwgylm.localhost/?studio-server=local&studio-parent=tauri%3A%2F%2Flocalhost&studio-origin=http%3A%2F%2Flocalhost&studio-navigation=combined";
      frame.hidden = true;
      document.body.append(frame);
      const result = await response;
      await window.__TAURI__.event.emit("studio-native-check-step", result);
      assert(
        !result.parentReadable,
        "The server view can read its privileged parent",
      );
      assert(!result.native, "The server view inherited native IPC");
      assert(
        result.origin !== location.origin,
        "The server view shares the shell origin",
      );
      assert(
        result.origin === result.messageOrigin &&
          result.origin === result.parsedOrigin,
        "Server view origin is ambiguous",
      );
      frame.remove();
      await window.__TAURI__.event.emit("studio-native-check-result", {
        passed: true,
        mode: "frames",
      });
      return;
    }
    if (window.__studioCheckMode.startsWith("drafts-offline")) {
      const identity = await call("cached-identity", "identity.get", {});
      const workspace = identity.result?.workspaceId;
      assert(workspace === "a".repeat(32), "Cached workspace was lost");
      const before = await call("offline-read", "drafts.view", { workspace });
      const expected =
        window.__studioCheckMode === "drafts-offline-write"
          ? "saved Ж🦀"
          : "offline Ж🦀";
      assert(
        before.result?.drafts.room === expected,
        "Offline draft recovery failed",
      );
      const edited = await call("offline-edit", "drafts.edit", {
        workspace,
        changes: { room: "offline Ж🦀" },
      });
      assert(
        edited.result?.drafts.room === "offline Ж🦀",
        "Offline edit failed",
      );
      await window.__TAURI__.event.emit("studio-native-check-result", {
        passed: true,
        mode: window.__studioCheckMode,
      });
      return;
    }
    if (window.__studioCheckMode.startsWith("network")) {
      if (window.__studioCheckMode.startsWith("network-drafts")) {
        const workspace = "a".repeat(32);
        let denied = false;
        try {
          // The app may already restore the cached verified identity on restart.
          // A different workspace must remain unauthorized in either ordering.
          await call("unopened-draft", "drafts.edit", {
            workspace: "c".repeat(32),
            changes: { room: "wrong" },
          });
        } catch {
          denied = true;
        }
        assert(denied, "An unopened workspace accepted a draft");
        const opened = await call("open-drafts", "sync.open", {});
        assert(
          opened.result?.workspaceId === workspace,
          "Workspace did not open",
        );
        denied = false;
        try {
          await call("other-workspace", "drafts.edit", {
            workspace: "b".repeat(32),
            changes: { room: "wrong" },
          });
        } catch {
          denied = true;
        }
        assert(denied, "Another workspace accepted a draft");
        const imported = await call("import-browser-drafts", "drafts.import", {
          workspace,
          versions: [
            {
              id: "browser:room",
              device: "browser",
              session: "room",
              text: "imported draft",
              updated: 1,
            },
          ],
        });
        assert(
          imported.result?.drafts.room === "imported draft",
          "Draft import receipt was lost",
        );
        if (window.__studioCheckMode === "network-drafts-write") {
          const edited = await call("draft-write", "drafts.edit", {
            workspace,
            changes: { room: "saved Ж🦀" },
          });
          assert(
            edited.result?.drafts.room === "saved Ж🦀",
            "Draft did not commit",
          );
        }
        const saved = await call("draft-read", "drafts.view", { workspace });
        assert(
          saved.result?.drafts.room === "saved Ж🦀",
          "Draft did not survive restart",
        );
        const refreshed = await call("draft-refresh", "drafts.refresh", {
          workspace,
        });
        assert(
          refreshed.result?.drafts.room === "saved Ж🦀" &&
            !refreshed.result.error,
          "Draft refresh failed",
        );
      } else if (window.__studioCheckMode === "network-stream") {
        let receive;
        const received = new Promise((resolve) => {
          receive = resolve;
        });
        const unlisten = await window.__TAURI__.event.listen(
          "studio-client-event",
          ({ payload }) => {
            if (
              payload.event === "fixture-events" &&
              payload.value?.data !== undefined
            )
              receive(payload.value);
          },
        );
        const subscribed = await call("sse-subscribe", "view.subscribe", {
          subscription: "fixture-events",
          topic: "sse",
          path: "/api/native-check-events",
        });
        assert(!subscribed.error, "Subscription was rejected");
        const event = await received;
        assert(
          event.type === "updated" &&
            event.id === "rev-1" &&
            event.data === "Привет 🦀\nsecond",
          "SSE event changed",
        );
        const closed = await call("sse-close", "view.unsubscribe", {
          subscription: "fixture-events",
        });
        assert(!closed.error, "Subscription did not close");
        unlisten();
      } else if (window.__studioCheckMode === "network-invalid") {
        let rejected = false;
        try {
          await call("bad-backend", "api.request", {
            path: "/api/native-check",
          });
        } catch {
          rejected = true;
        }
        assert(rejected, "Wrong backend state was accepted");
      } else {
        const response = await call("http-command", "api.request", {
          path: "/api/native-check",
          method: "POST",
          body: null,
          requestId: "original-http-command",
          apiSchema: "fixture-client-schema",
          workspaceId: "fixture-workspace",
          httpResponse: true,
        });
        assert(!response.error, JSON.stringify(response.error));
        assert(response.result.status === 409, "HTTP status was lost");
        assert(
          response.result.headers["x-studio-api-schema-mismatch"] === "1",
          "Schema mismatch was lost",
        );
        assert(response.result.base64 === "YQD/", "Binary response changed");
      }
      await window.__TAURI__.event.emit("studio-native-check-result", {
        passed: true,
        mode: window.__studioCheckMode,
      });
      return;
    }
    const snapshot = {
      version: 1,
      entries: { "imported-draft": "legacy Ж🦀" },
    };
    if (window.__studioCheckMode === "write") {
      const imported = await call(
        "stable-import",
        "preferences.import",
        snapshot,
      );
      assert(imported.result?.imported === 1, "Snapshot did not import");
      const edited = await call("edit-imported", "preferences.raw_set", {
        key: "imported-draft",
        raw: "newer Ж🦀",
      });
      assert(!edited.error, "Imported value could not be edited");
    }
    const replay = await call("stable-import", "preferences.import", snapshot);
    assert(
      replay.result?.imported === 1,
      "Import receipt did not survive restart",
    );
    const importedRead = await call("read-imported", "preferences.raw_all");
    assert(
      importedRead.result?.["imported-draft"] === "newer Ж🦀",
      "Import replay erased a newer edit",
    );
    const value = "persistent Ж🦀";
    if (window.__studioCheckMode === "write") {
      const params = { key: "native-check", raw: value };
      const written = await call("stable-write", "preferences.raw_set", params);
      assert(!written.error, "Native write failed");
      const replayed = await call(
        "stable-write",
        "preferences.raw_set",
        params,
      );
      assert(!replayed.error, "Exact retry failed");
      const changed = await call("stable-write", "preferences.raw_set", {
        ...params,
        raw: "changed",
      });
      assert(changed.error, "Changed request identity was accepted");
    }
    const read = await call("read-check", "preferences.raw_all");
    assert(
      read.result?.["native-check"] === value,
      "Native persistent value changed",
    );
    // Exercise the real renderer bootstrap and Storage caller, not just IPC.
    for (
      let attempt = 0;
      attempt < 100 && typeof window.localStorage.flush !== "function";
      attempt++
    )
      await new Promise((resolve) => setTimeout(resolve, 50));
    assert(
      typeof window.localStorage.flush === "function",
      "Native preference bootstrap did not complete",
    );
    if (window.__studioCheckMode === "write") {
      window.localStorage.setItem("renderer-check", value);
      await window.localStorage.flush();
    }
    assert(
      window.localStorage.getItem("renderer-check") === value,
      "Renderer preference did not survive WebView restart",
    );
    const browserRead = await call(
      "renderer-read-check",
      "preferences.raw_all",
    );
    assert(
      browserRead.result?.['browser:"root":renderer-check'] === value,
      "Renderer preference was not committed to Rust",
    );
    let denied = false;
    try {
      await call("network-denied", "api.request", { path: "/api/health" });
    } catch {
      denied = true;
    }
    assert(denied, "Unconnected network service was allowed");
    await window.__TAURI__.event.emit("studio-native-check-result", {
      passed: true,
      mode: window.__studioCheckMode,
    });
  } catch (error) {
    await window.__TAURI__.event.emit("studio-native-check-result", {
      passed: false,
      error: String(error),
    });
  }
})();
