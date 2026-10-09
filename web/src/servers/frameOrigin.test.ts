import { expect, it } from "vitest";
import { createRequire } from "node:module";
import { frameURL, serverFrameHost } from "./frameOrigin";
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
it.each(["classic", "combined"])(
  "accepts the renderer's %s frame URL at the native boundary",
  (navigation) => {
    const shell = "http://127.0.0.1:4620";
    const assets = "http://127.0.0.1:4621";
    const url = frameURL(
      { id: "local", label: "This computer", origin: shell },
      `${shell}/?studio-navigation=${navigation}`,
      assets,
    );
    expect(native.frameOwner(url, assets, shell)).toBe("local");
    for (const query of [
      "studio-navigation=invalid",
      "studio-navigation=classic&studio-navigation=combined",
      "unknown=classic",
      "studio-parent=http://other.localhost:4620",
      "studio-server=remote",
    ]) {
      const invalid = new URL(url);
      const keys = new Set(new URLSearchParams(query).keys());
      for (const key of keys) invalid.searchParams.delete(key);
      for (const [key, value] of new URLSearchParams(query))
        invalid.searchParams.append(key, value);
      expect(native.frameOwner(invalid.href, assets, shell)).toBeUndefined();
    }
  },
);
it.each(["", "?studio-navigation=combined", "?studio-navigation=invalid"])(
  "uses combined frames for the default and harmless diagnostic URLs (%s)",
  (query) => {
    const url = frameURL(
      { id: "local", label: "This computer", origin: "http://127.0.0.1:4620" },
      "http://127.0.0.1:4620/" + query,
      "http://127.0.0.1:4621",
    );
    expect(new URL(url).searchParams.get("studio-navigation")).toBe("combined");
  },
);
