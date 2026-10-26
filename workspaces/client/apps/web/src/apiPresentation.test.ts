import { build } from "esbuild";
import { fileURLToPath } from "node:url";
import { expect, it, vi } from "vitest";
import * as legacyApi from "./api";
import * as nativeApi from "./native/api";
import {
  ApiError,
  ApiSchemaMismatchError,
  NetworkTimeoutError,
} from "./apiErrors";

it("the native API exports the same presentation and wait functions as legacy callers", () => {
  expect(nativeApi.errorText).toBeTypeOf("function");
  expect(nativeApi.untilAnswered).toBeTypeOf("function");
  expect(nativeApi.errorText).toBe(legacyApi.errorText);
  expect(nativeApi.untilAnswered).toBe(legacyApi.untilAnswered);
  expect(nativeApi.ApiError).toBe(legacyApi.ApiError);
  expect(nativeApi.NetworkTimeoutError).toBe(legacyApi.NetworkTimeoutError);
});

it("the native API runtime import graph excludes browser networking", async () => {
  const result = await build({
    entryPoints: [fileURLToPath(new URL("./native/api.ts", import.meta.url))],
    bundle: true,
    write: false,
    platform: "node",
    format: "esm",
    metafile: true,
    logLevel: "silent",
  });
  const inputs = Object.keys(result.metafile!.inputs);
  expect(inputs.some((path) => path.endsWith("/apiPresentation.ts"))).toBe(
    true,
  );
  for (const path of inputs) {
    expect(path).not.toMatch(/(?:^|\/)src\/api\.ts$/);
    expect(path).not.toMatch(/openapi-fetch|servers\/transport|sync\/client/);
  }
});

it("preserves shared error identities and exact presentation", () => {
  expect(nativeApi.errorText(new ApiSchemaMismatchError())).toBe("");
  expect(
    nativeApi.errorText(
      new ApiError("Invalid request", 400, {}, ["body", "id"]),
    ),
  ).toBe("Invalid request (body.id).");
  expect(nativeApi.errorText(new NetworkTimeoutError())).toBe(
    "The server did not respond in time.",
  );
  expect(
    nativeApi.errorText({
      error: { message: "Provider refused the request." },
    }),
  ).toBe("Provider refused the request.");
});

it("keeps the existing wait behavior for shared timeout errors only", async () => {
  const send = vi
    .fn()
    .mockRejectedValueOnce(new NetworkTimeoutError())
    .mockRejectedValueOnce(new NetworkTimeoutError())
    .mockResolvedValueOnce("answered");
  await expect(nativeApi.untilAnswered(send)).resolves.toBe("answered");
  expect(send).toHaveBeenCalledTimes(3);
  for (const failure of [
    new TypeError("connection lost"),
    new ApiError("Refused", 400),
  ]) {
    const refused = vi.fn().mockRejectedValue(failure);
    await expect(nativeApi.untilAnswered(refused)).rejects.toBe(failure);
    expect(refused).toHaveBeenCalledOnce();
  }
});
