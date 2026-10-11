import { afterEach, describe, expect, it, vi } from "vitest";
import { openSignInPage } from "./openSignIn";

const LINK = "https://accounts.example.com/authorize?client_id=kube";

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("openSignInPage", () => {
  it("uses the desktop sign-in action, which brings the browser to the front", async () => {
    const openSignIn = vi.fn(async () => {});
    const openExternal = vi.fn(async () => {});
    vi.stubGlobal("window", { codexDesktop: { openSignIn, openExternal } });
    await openSignInPage(LINK);
    expect(openSignIn).toHaveBeenCalledWith(LINK);
    expect(openExternal).not.toHaveBeenCalled();
  });

  it("falls back to openExternal on an older desktop app", async () => {
    const openExternal = vi.fn(async () => {});
    vi.stubGlobal("window", { codexDesktop: { openExternal } });
    await openSignInPage(LINK);
    expect(openExternal).toHaveBeenCalledWith(LINK);
  });

  it("opens a new tab without an opener in a plain browser", async () => {
    const tab = { opener: {} as unknown };
    const open = vi.fn(() => tab);
    vi.stubGlobal("window", { open });
    await openSignInPage(LINK);
    expect(open).toHaveBeenCalledWith(LINK, "_blank");
    expect(tab.opener).toBeNull();
    vi.stubGlobal("window", { open: () => null });
    await expect(openSignInPage(LINK)).rejects.toThrow("Allow pop-ups");
  });

  it("refuses other schemes before any open", async () => {
    const open = vi.fn();
    vi.stubGlobal("window", { open });
    await expect(openSignInPage("javascript:alert(1)")).rejects.toThrow(
      "Only HTTP and HTTPS",
    );
    expect(open).not.toHaveBeenCalled();
  });
});
