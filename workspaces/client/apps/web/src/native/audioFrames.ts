// The shell owns every native audio identity. A server view cannot observe,
// stop, or transcribe another view's recording, even with a copied token.
type Invoke = (
  method: string,
  params: unknown,
  id?: string,
) => Promise<unknown>;
type Listen = (callback: (value: unknown) => void) => Promise<() => void>;
type AudioEvent = { event: string; value: unknown };
type Caller =
  | { frame: HTMLIFrameElement; origin: string; live: () => boolean }
  | {
      owner: object;
      deliver: (event: AudioEvent) => void;
      live: () => boolean;
    };
type Subscription = {
  caller: Caller;
  logical: string;
  capture?: string | undefined;
  voice?: string | undefined;
};
export const audioFrameMethod = (method: string, params: unknown) =>
  [
    "desktop.requestMicrophone",
    "desktop.prepareTranscription",
    "desktop.cancelTranscription",
    "media.captureStart",
    "media.captureStop",
    "voice.create",
    "voice.answer",
    "voice.mute",
    "voice.close",
    "speech.uploadBegin",
    "speech.uploadChunk",
    "speech.uploadCancel",
    "speech.transcribe",
    "audio.unsubscribe",
  ].includes(method) ||
  (method === "view.subscribe" &&
    !!params &&
    typeof params === "object" &&
    "topic" in params &&
    typeof params.topic === "string" &&
    ["microphone", "transcription", "voice"].includes(params.topic));

export function bindAudioFrames(invoke: Invoke, listen?: Listen) {
  const permits = new Map<string, Caller>();
  const uploads = new Map<string, Caller>();
  const running = new Map<string, Caller>();
  const subscriptions = new Map<string, Subscription>();
  let microphone: { caller: Caller; expires: number } | undefined;
  let capture: { caller: Caller; id: string; voice?: boolean } | undefined;
  let closed = false;
  let listening: Promise<() => void> | undefined;
  const identity = (caller: Caller) =>
    "frame" in caller ? caller.frame : caller.owner;
  const same = (a: Caller | undefined, b: Caller) =>
    !!a && identity(a) === identity(b) && a.live() && b.live();
  const owns = (owner: Caller | undefined, caller: Caller) => {
    if (closed || !same(owner, caller))
      throw new Error(
        "This audio operation belongs to another view or has expired.",
      );
  };
  const quiet = (method: string, params: unknown) =>
    void invoke(method, params).catch(() => {});
  const prune = () => {
    const dead = (caller: Caller) => closed || !caller.live();
    for (const [id, row] of subscriptions)
      if (dead(row.caller)) {
        subscriptions.delete(id);
        quiet("view.unsubscribe", { subscription: id });
      }
    for (const [id, caller] of uploads)
      if (dead(caller)) {
        uploads.delete(id);
        quiet("speech.uploadCancel", { transfer: id });
      }
    for (const [id, caller] of permits) if (dead(caller)) permits.delete(id);
    // Closing a frame cancels only its operation; cancellation needs no new
    // gesture because the host verifies the attempt capability separately.
    for (const [id, caller] of running)
      if (dead(caller)) quiet("speech.cancel", { id });
    if (capture && dead(capture.caller)) {
      quiet(
        capture.voice ? "voice.close" : "media.captureStop",
        capture.voice ? { session: capture.id } : { capture: capture.id },
      );
      capture = undefined;
    }
    if (microphone && dead(microphone.caller)) microphone = undefined;
  };
  const observer =
    typeof MutationObserver === "function"
      ? new MutationObserver(prune)
      : undefined;
  if (observer && typeof document !== "undefined")
    observer.observe(document.documentElement, {
      childList: true,
      subtree: true,
    });
  async function subscribeEvents() {
    if (!listen) throw new Error("Native audio events are unavailable.");
    listening ??= listen((value) => {
      if (
        !value ||
        typeof value !== "object" ||
        !("event" in value) ||
        typeof value.event !== "string" ||
        !("value" in value)
      )
        return;
      const row = subscriptions.get(value.event);
      if (!row || !row.caller.live() || closed) return;
      if (
        !row.capture &&
        !row.voice &&
        !same(
          running.get(
            value.value &&
              typeof value.value === "object" &&
              "id" in value.value &&
              typeof value.value.id === "string"
              ? value.value.id
              : "",
          ),
          row.caller,
        )
      )
        return;
      const event = { event: row.logical, value: value.value };
      if ("frame" in row.caller)
        row.caller.frame.contentWindow?.postMessage(
          { kind: "studio-native-audio-event", ...event },
          row.caller.origin,
        );
      else row.caller.deliver(event);
    });
    await listening;
  }
  const router = {
    // Each mounted server connection receives an opaque owner, independent of
    // selection and DOM nodes. Closing it releases only its audio resources.
    createClient() {
      let live = true;
      const listeners = new Set<(event: AudioEvent) => void>();
      const caller: Caller = {
        owner: {},
        live: () => live,
        deliver: (event) => {
          for (const listener of listeners) listener(event);
        },
      };
      return {
        request(
          method: string,
          params: unknown,
          id: string = crypto.randomUUID(),
        ) {
          return router.request(
            method === "view.unsubscribe" ? "audio.unsubscribe" : method,
            params,
            id,
            caller,
          );
        },
        async listen(listener: (event: AudioEvent) => void) {
          if (!live || closed) throw new Error("The server view closed.");
          listeners.add(listener);
          return () => {
            listeners.delete(listener);
          };
        },
        close() {
          live = false;
          listeners.clear();
          prune();
        },
      };
    },
    async request(
      method: string,
      input: unknown,
      id: string,
      caller: Caller,
    ): Promise<unknown> {
      if (!input || typeof input !== "object")
        throw new Error("Invalid audio parameters.");
      const params = input as Record<string, unknown>;
      const text = (key: string): string => {
        const value = params[key];
        if (typeof value !== "string")
          throw new Error("Invalid audio identity.");
        return value;
      };
      prune();
      if (closed || !caller.live()) throw new Error("The server view closed.");
      if (method === "desktop.requestMicrophone") {
        if (
          (capture && !same(capture.caller, caller)) ||
          (microphone &&
            !same(microphone.caller, caller) &&
            microphone.expires > Date.now())
        )
          throw new Error("Another server view is using the microphone.");
        microphone = { caller, expires: Infinity };
        try {
          const result = await invoke(method, {}, id);
          if (microphone?.caller === caller)
            microphone.expires = Date.now() + 15_000;
          return result;
        } catch (error) {
          if (microphone?.caller === caller) microphone = undefined;
          throw error;
        }
      }
      if (method === "desktop.prepareTranscription") {
        const permit = await invoke(method, {}, id);
        if (typeof permit !== "string")
          throw new Error("Invalid speech permit.");
        if (caller.live() && !closed) permits.set(permit, caller);
        return permit;
      }
      if (method === "view.subscribe") {
        await subscribeEvents();
        if (
          typeof params.subscription !== "string" ||
          !params.subscription ||
          params.subscription.length > 128 ||
          [...subscriptions.values()].some(
            (row) =>
              same(row.caller, caller) && row.logical === params.subscription,
          )
        )
          throw new Error("Invalid or duplicate audio subscription.");
        if (params.topic === "microphone" || params.topic === "voice") {
          owns(microphone?.caller, caller);
          if (capture)
            throw new Error("A microphone capture is already active.");
          if (
            typeof (params.topic === "voice"
              ? params.session
              : params.capture) !== "string" ||
            !text(params.topic === "voice" ? "session" : "capture") ||
            text(params.topic === "voice" ? "session" : "capture").length > 128
          )
            throw new Error("Invalid capture.");
        }
        const nativeId = crypto.randomUUID();
        subscriptions.set(nativeId, {
          caller,
          logical: params.subscription,
          capture: params.topic === "microphone" ? text("capture") : undefined,
          voice: params.topic === "voice" ? text("session") : undefined,
        });
        try {
          const result = await invoke(
            method,
            {
              subscription: nativeId,
              topic: params.topic,
              capture: nativeId,
              session: nativeId,
            },
            id,
          );
          if (closed || !caller.live())
            quiet("view.unsubscribe", { subscription: nativeId });
          prune();
          return result;
        } catch (error) {
          subscriptions.delete(nativeId);
          throw error;
        }
      }
      if (method === "audio.unsubscribe") {
        const entry = [...subscriptions].find(
          ([, row]) =>
            same(row.caller, caller) && row.logical === params.subscription,
        );
        if (!entry) throw new Error("Unknown audio subscription.");
        if (capture?.id === entry[0]) {
          quiet(
            capture.voice ? "voice.close" : "media.captureStop",
            capture.voice ? { session: capture.id } : { capture: capture.id },
          );
          capture = undefined;
          microphone = undefined;
        }
        subscriptions.delete(entry[0]);
        return invoke("view.unsubscribe", { subscription: entry[0] }, id);
      }
      if (
        ["voice.create", "voice.answer", "voice.mute", "voice.close"].includes(
          method,
        )
      ) {
        const entry = [...subscriptions].find(
          ([, row]) => same(row.caller, caller) && row.voice === params.session,
        );
        if (!entry) throw new Error("Unknown voice session.");
        if (method === "voice.create") {
          owns(microphone?.caller, caller);
          if (!microphone || microphone.expires <= Date.now() || capture)
            throw new Error("Use the Voice button again.");
          capture = { caller, id: entry[0], voice: true };
        } else if (method !== "voice.close") {
          owns(capture?.caller, caller);
          if (capture?.id !== entry[0] || !capture.voice)
            throw new Error("Unknown active voice session.");
        }
        try {
          return await invoke(
            method,
            { session: entry[0], sdp: params.sdp, muted: params.muted },
            id,
          );
        } catch (error) {
          if (method === "voice.create" && capture?.id === entry[0])
            capture = undefined;
          throw error;
        } finally {
          if (method === "voice.close" && capture?.id === entry[0]) {
            capture = undefined;
            microphone = undefined;
          }
          if (method === "voice.create" && (closed || !caller.live()))
            quiet("voice.close", { session: entry[0] });
          prune();
        }
      }
      if (method === "media.captureStart" || method === "media.captureStop") {
        const entry = [...subscriptions].find(
          ([, row]) =>
            same(row.caller, caller) && row.capture === params.capture,
        );
        if (!entry) throw new Error("Unknown microphone capture.");
        if (method === "media.captureStart") {
          owns(microphone?.caller, caller);
          if (!microphone || microphone.expires <= Date.now() || capture)
            throw new Error("Use the Record button again.");
          capture = { caller, id: entry[0] };
        } else owns(capture?.caller, caller);
        try {
          return await invoke(method, { capture: entry[0] }, id);
        } catch (error) {
          if (method === "media.captureStart") capture = undefined;
          throw error;
        } finally {
          if (method === "media.captureStop") {
            capture = undefined;
            microphone = undefined;
          }
          if (method === "media.captureStart" && (closed || !caller.live()))
            quiet("media.captureStop", { capture: entry[0] });
          prune();
        }
      }
      if (method === "speech.uploadBegin") {
        owns(permits.get(text("permit")), caller);
        const result = (await invoke(
          method,
          { permit: params.permit, size: params.size },
          id,
        )) as { transfer: string };
        if (!caller.live() || closed) quiet("speech.uploadCancel", result);
        else uploads.set(result.transfer, caller);
        return result;
      }
      if (method === "speech.uploadChunk" || method === "speech.uploadCancel") {
        owns(uploads.get(text("transfer")), caller);
        try {
          return await invoke(method, params, id);
        } finally {
          if (method === "speech.uploadCancel")
            uploads.delete(text("transfer"));
        }
      }
      if (method === "desktop.cancelTranscription") {
        owns(running.get(text("value")), caller);
        return invoke(method, { value: params.value }, id);
      }
      if (method === "speech.transcribe") {
        owns(uploads.get(text("transfer")), caller);
        owns(permits.get(text("permit")), caller);
        if (
          typeof params.id !== "string" ||
          !params.id ||
          params.id.length > 128 ||
          running.has(params.id)
        )
          throw new Error("Invalid transcription attempt.");
        permits.delete(text("permit"));
        running.set(params.id, caller);
        try {
          return await invoke(
            method,
            {
              id: params.id,
              transfer: params.transfer,
              permit: params.permit,
              locale: params.locale,
            },
            id,
          );
        } finally {
          running.delete(params.id);
          uploads.delete(text("transfer"));
          quiet("speech.uploadCancel", { transfer: params.transfer });
        }
      }
      throw new Error("Unknown audio command.");
    },
    dispose() {
      closed = true;
      observer?.disconnect();
      prune();
      void listening?.then((stop) => stop()).catch(() => {});
    },
  };
  return router;
}
