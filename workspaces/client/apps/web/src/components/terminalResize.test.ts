import { describe, expect, it } from "vitest";
import { TerminalResizeGesture } from "./terminalResize";

describe("terminal resize completion", () => {
  it("commits only after the owner completes a changed drag", () => {
    const drag = new TerminalResizeGesture();
    drag.start("local", 1, 500, 360);
    expect(drag.move("local", 1, 450, 100, 700)).toBe(410);
    expect(drag.finish("local", 2)).toBeNull();
    expect(drag.finish("local", 1)).toBe(410);
    expect(drag.finish("local", 1)).toBeNull();
  });
  it("does not overwrite a clamped preference after a click", () => {
    const drag = new TerminalResizeGesture();
    drag.start("local", 1, 500, 140);
    expect(drag.finish("local", 1)).toBeNull();
  });
  it("does not write after cancellation, lost capture, or a server switch", () => {
    const drag = new TerminalResizeGesture();
    drag.start("local", 1, 500, 360);
    drag.move("local", 1, 450, 100, 700);
    drag.cancel();
    expect(drag.finish("local", 1)).toBeNull();
    drag.start("local", 1, 500, 360);
    drag.move("local", 1, 450, 100, 700);
    expect(drag.finish("remote", 1)).toBeNull();
    expect(drag.finish("local", 1)).toBeNull();
  });
  it("ignores movement from a different pointer or scope", () => {
    const drag = new TerminalResizeGesture();
    drag.start("local", 1, 500, 360);
    expect(drag.move("remote", 1, 450, 100, 700)).toBeNull();
    expect(drag.move("local", 2, 450, 100, 700)).toBeNull();
    expect(drag.move("local", 1, 0, 142, 142)).toBe(142);
  });
});
