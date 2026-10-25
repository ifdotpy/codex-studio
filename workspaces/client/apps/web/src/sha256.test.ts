import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createHash, webcrypto } from "node:crypto";
import { readFileSync } from "node:fs";
import { sha256Hex } from "./sha256";

const encode = (value: string) => new TextEncoder().encode(value);
const vectors = [
  ["empty", new Uint8Array()],
  ["ASCII", encode("abc")],
  ["non-ASCII", encode("Привет 🦀")],
  ["block boundary", encode("a".repeat(64))],
  ["byte view", new Uint8Array([99, 0, 128, 255, 98]).subarray(1, 4)],
] as const;

beforeEach(() => {
  vi.stubGlobal(
    "fetch",
    vi.fn(
      async () =>
        new Response(
          readFileSync(
            new URL("./wasm/generated/client_bg.wasm", import.meta.url),
          ),
          { headers: { "Content-Type": "application/wasm" } },
        ),
    ),
  );
});
afterEach(() => vi.unstubAllGlobals());

describe("browser SHA-256 content identity owner", () => {
  it.each(vectors)(
    "matches node:crypto through compiled WASM and native digest: %s",
    async (_name, bytes) => {
      const expected = createHash("sha256").update(bytes).digest("hex");
      vi.stubGlobal("crypto", {});
      const fallback = await sha256Hex(bytes);
      expect(fallback).toBe(expected);
      vi.stubGlobal("crypto", webcrypto);
      expect(await sha256Hex(bytes)).toBe(fallback);
    },
  );

  it("uses the native digest with its receiver and no WASM request", async () => {
    const subtle = {
      digest: vi.fn(function (
        this: unknown,
        algorithm: string,
        bytes: Uint8Array<ArrayBuffer>,
      ) {
        expect(this).toBe(subtle);
        return webcrypto.subtle.digest(algorithm, bytes);
      }),
    };
    vi.stubGlobal("crypto", { subtle });
    expect(await sha256Hex(encode("abc"))).toBe(
      createHash("sha256").update("abc").digest("hex"),
    );
    expect(subtle.digest).toHaveBeenCalledOnce();
    expect(fetch).not.toHaveBeenCalled();
  });

  it("does not mask a native digest failure with WASM", async () => {
    const failure = new Error("Native digest failed");
    vi.stubGlobal("crypto", {
      subtle: { digest: vi.fn().mockRejectedValue(failure) },
    });
    await expect(sha256Hex(encode("abc"))).rejects.toBe(failure);
    expect(fetch).not.toHaveBeenCalled();
  });

  it.each([undefined, { subtle: {} }])(
    "supports an absent digest without changing the bytes (%s)",
    async (crypto) => {
      vi.stubGlobal("crypto", crypto);
      const bytes = encode("original");
      const pending = sha256Hex(bytes);
      bytes.fill(0);
      expect(await pending).toBe(
        createHash("sha256").update("original").digest("hex"),
      );
    },
  );
});
