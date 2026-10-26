import { describe, expect, it } from "vitest";
import { resolveSidebarWidth, storedSidebarWidth } from "./sidebarWidth";
import { SidebarResizeSession } from "./sidebarResize";

// Deliberately distinct fixture values check the policy's input contract.
const contract = {
  minimumRem: 12,
  preferredRem: 16,
  conversationMinRem: 38,
  navigationInlineMinPx: 750,
};
const input = {
  hostWidth: 1400,
  rootFontSize: 16,
  headerMinimum: 220,
  preferred: 320,
  contract,
};

describe("sidebar width policy", () => {
  it.each([null, "", "320", NaN, Infinity, -1, 0, {}, []])(
    "rejects invalid stored width %j",
    (value) => expect(storedSidebarWidth(value)).toBeNull(),
  );

  it("keeps the preference after a temporary host clamp", () => {
    expect(resolveSidebarWidth(input).width).toBe(320);
    const small = resolveSidebarWidth({ ...input, hostWidth: 900 });
    expect(small).toMatchObject({ mode: "inline", width: 292 });
    expect(input.preferred).toBe(320);
    expect(resolveSidebarWidth(input).width).toBe(320);
  });

  it("uses the measured header minimum and the current root size", () => {
    expect(resolveSidebarWidth({ ...input, preferred: 1 }).minimum).toBe(220);
    expect(
      resolveSidebarWidth({ ...input, rootFontSize: 24, preferred: 1 }),
    ).toMatchObject({ minimum: 288, width: 288 });
    expect(
      resolveSidebarWidth({ ...input, headerMinimum: 301.2, preferred: 1 })
        .width,
    ).toBe(302);
  });

  it("changes mode before the maximum becomes smaller than the minimum", () => {
    const boundary = 220 + 38 * 16;
    expect(resolveSidebarWidth({ ...input, hostWidth: boundary }).mode).toBe(
      "inline",
    );
    expect(
      resolveSidebarWidth({ ...input, hostWidth: boundary - 0.1 }).mode,
    ).toBe("drawer");
    expect(resolveSidebarWidth({ ...input, hostWidth: 749.9 }).mode).toBe(
      "drawer",
    );
  });

  it("uses the default for invalid preferences and measurements", () => {
    expect(resolveSidebarWidth({ ...input, preferred: NaN }).width).toBe(256);
    for (const hostWidth of [NaN, Infinity, -1, 0])
      expect(resolveSidebarWidth({ ...input, hostWidth }).mode).toBe("drawer");
  });
});

describe("sidebar resize lifecycle", () => {
  it("does not replace a temporarily clamped preference after a click without movement", () => {
    const session = new SidebarResizeSession();
    session.start("a", 1, 240, 240);
    expect(
      session.finish("a", 1, 240, { minimum: 220, maximum: 260 }),
    ).toBeNull();
  });

  it("bounds both ends and commits only the final pointer position", () => {
    const session = new SidebarResizeSession();
    const bounds = { minimum: 220, maximum: 700 };
    session.start("a", 1, 320, 320);
    expect(session.move("a", 1, -1000, bounds)).toBe(220);
    expect(session.move("a", 1, 2000, bounds)).toBe(700);
    expect(session.finish("a", 1, 400, bounds)).toBe(400);
    expect(session.finish("a", 1, 400, bounds)).toBeNull();
  });

  it("cancels on lost capture, escape, and scope change without a commit", () => {
    const session = new SidebarResizeSession();
    const bounds = { minimum: 220, maximum: 700 };
    for (const reason of ["lost capture", "escape"]) {
      session.start("a", 1, 320, 320);
      session.move("a", 1, 400, bounds);
      session.cancel();
      expect(session.finish("a", 1, 400, bounds), reason).toBeNull();
    }
    session.start("a", 1, 320, 320);
    expect(session.finish("b", 1, 400, bounds)).toBeNull();
    expect(session.finish("a", 1, 400, bounds)).toBeNull();
  });

  it("ignores a different pointer", () => {
    const session = new SidebarResizeSession();
    session.start("a", 1, 320, 320);
    expect(
      session.move("a", 2, 400, { minimum: 220, maximum: 700 }),
    ).toBeNull();
  });
});
