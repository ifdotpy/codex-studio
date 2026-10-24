/** A pointer drag writes its height only on completion in its original server. */
export class TerminalResizeGesture {
  private drag: {
    scope: string;
    pointer: number;
    y: number;
    start: number;
    height: number;
  } | null = null;

  start(scope: string, pointer: number, y: number, height: number) {
    this.drag = { scope, pointer, y, start: height, height };
  }

  move(scope: string, pointer: number, y: number, min: number, max: number) {
    const drag = this.drag;
    if (!drag || drag.scope !== scope || drag.pointer !== pointer) return null;
    drag.height = Math.min(max, Math.max(min, drag.start + drag.y - y));
    return drag.height;
  }

  finish(scope: string, pointer: number) {
    const drag = this.drag;
    if (!drag || drag.pointer !== pointer) return null;
    this.drag = null;
    return drag.scope === scope && drag.height !== drag.start
      ? drag.height
      : null;
  }

  cancel() {
    this.drag = null;
  }
}
