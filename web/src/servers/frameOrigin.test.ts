import { expect, it } from "vitest";
import { createRequire } from "node:module";
import { serverFrameHost } from "./frameOrigin";
const native = createRequire(import.meta.url)(
  "../../../desktop/frame-owner.cjs",
);
it("uses distinct DNS-valid frame hosts for case-sensitive IDs, UUIDs and maximum length IDs", () => {
  const ids = [
    "local",
    "remote",
    "A",
    "a",
    "7d265689-5a48-4aaa-85d6-54e0510fbb83",
    "a".repeat(128),
  ];
  expect(new Set(ids.map(serverFrameHost)).size).toBe(ids.length);
  for (const id of ids) {
    const host = serverFrameHost(id);
    expect(host.length).toBeLessThanOrEqual(253);
    expect(host.split(".").every((label) => label.length <= 63)).toBe(true);
    expect(native.serverFrameHost(id)).toBe(host);
    const url = `http://${host}:4621/?${new URLSearchParams({ "studio-server": id, "studio-parent": "http://127.0.0.1:4621" })}`;
    expect(
      native.frameOwner(url, "http://127.0.0.1:4621", "http://127.0.0.1:4621"),
    ).toBe(id);
    expect(
      native.frameOwner(
        url.replace("studio-server=" + id, "studio-server=other"),
        "http://127.0.0.1:4621",
        "http://127.0.0.1:4621",
      ),
    ).toBeUndefined();
  }
});
