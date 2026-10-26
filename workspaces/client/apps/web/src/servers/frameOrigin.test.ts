import { frameMessageOrigin } from "./frameOrigin";
import { expect, it } from "vitest";
import { createRequire } from "node:module";
import { frameURL, serverFrameHost } from "./frameOrigin";
const native = createRequire(import.meta.url)(
  "../../../desktop/frame-owner.ts",
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

it("encodes server IDs as lowercase unpadded base32 in labels of at most 50 characters", () => {
  for (const [id, host] of [
    ["local", "studio-nrxwgylm.localhost"],
    ["remote", "studio-ojsw233umu.localhost"],
    ["A", "studio-ie.localhost"],
    ["a", "studio-me.localhost"],
    [
      "7d265689-5a48-4aaa-85d6-54e0510fbb83",
      "studio-g5sdenrvgy4dsljvme2dqljumfqwcljygvsdmljvgrstanjrgb.tgeyrygm.localhost",
    ],
  ] as const) {
    expect(serverFrameHost(id), id).toBe(host);
  }
});

it("gives native server views separate bundled origins", () => {
  const url = new URL(
    frameURL(
      { id: "local", label: "Local", origin: "http://localhost" },
      "tauri://localhost/index.html",
      "",
    ),
  );
  expect(url.protocol).toBe("studio-view:");
  expect(url.hostname).toBe("studio-nrxwgylm.localhost");
  expect(url.searchParams.get("studio-server")).toBe("local");
});

it("uses exact WebKit origins without collapsing native hosts to null", () => {
  expect(frameMessageOrigin("studio-view://studio-one.localhost/?q=x")).toBe(
    "studio-view://studio-one.localhost",
  );
  expect(frameMessageOrigin("studio-view://studio-two.localhost/")).not.toBe(
    frameMessageOrigin("studio-view://studio-one.localhost/"),
  );
  expect(frameMessageOrigin("tauri://localhost/index.html")).toBe(
    "tauri://localhost",
  );
  expect(frameMessageOrigin("http://localhost:4621/")).toBe(
    "http://localhost:4621",
  );
});
