import { afterEach, expect, it, vi } from "vitest";
import { openNativeLink } from "./externalLinks";

afterEach(() => vi.unstubAllGlobals());
function host() {
  const invoke = vi.fn(async (_command, { request }) => ({
    id: request.id,
    result: true,
  }));
  vi.stubGlobal("__TAURI__", { core: { invoke } });
  return invoke;
}
it("uses separate foreground sign-in and background link actions", async () => {
  const invoke = host();
  await openNativeLink("https://example.com/page");
  await openNativeLink("https://example.com/login", true);
  expect(invoke.mock.calls.map(([, value]) => value.request.method)).toEqual([
    "desktop.openExternal",
    "desktop.openSignIn",
  ]);
});
it.each([
  "file:///tmp/file",
  "javascript:alert(1)",
  "https://user:secret@example.com/",
])("rejects unsafe link %s before invoking the host", async (url) => {
  const invoke = host();
  await expect(openNativeLink(url)).rejects.toThrow("Only HTTP and HTTPS");
  expect(invoke).not.toHaveBeenCalled();
});
it("does not retry after an uncertain native reply", async () => {
  const invoke = host();
  invoke.mockRejectedValueOnce(new Error("connection closed"));
  await expect(openNativeLink("https://example.com/")).rejects.toThrow(
    "connection closed",
  );
  expect(invoke).toHaveBeenCalledOnce();
});
