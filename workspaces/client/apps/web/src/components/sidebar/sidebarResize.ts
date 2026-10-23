import { clampSidebarWidth } from "./sidebarWidth";

type Bounds = { minimum: number; maximum: number };

/** One local pointer gesture. Only finish can produce a preference write. */
export class SidebarResizeSession {
  private drag: {
    scope: string;
    pointer: number;
    x: number;
    width: number;
  } | null = null;

  start(scope: string, pointer: number, x: number, width: number) {
    this.drag = { scope, pointer, x, width };
  }

  move(scope: string, pointer: number, x: number, bounds: Bounds) {
    const drag = this.drag;
    if (!drag || drag.pointer !== pointer) return null;
    if (drag.scope !== scope) {
      this.cancel();
      return null;
    }
    return clampSidebarWidth(
      drag.width + x - drag.x,
      bounds.minimum,
      bounds.maximum,
    );
  }

  finish(scope: string, pointer: number, x: number, bounds: Bounds) {
    const width = this.move(scope, pointer, x, bounds);
    if (this.drag?.pointer === pointer) this.cancel();
    return width;
  }

  cancel() {
    this.drag = null;
  }
}
