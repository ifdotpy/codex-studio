import type { ITerminalOptions } from "@xterm/xterm";
import type { Root } from "react-dom/client";
import type { mount } from "./terminal-input-fixture.ts";
declare global {
  interface Window {
    terminals: Terminal[];
    currentTerminal: Terminal;
    retiredTerminal: Terminal;
    reactRoot: Root | null;
    notices: string[];
    advance: number;
    terminalInputRecoveryMount: typeof mount;
  }
}
export class Terminal {
  declare options: ITerminalOptions;
  declare cols: number;
  declare rows: number;
  declare disposed: boolean | undefined;
  declare input: ((text: string) => void) | undefined;
  constructor(options: ITerminalOptions) {
    this.options = options;
    this.cols = 100;
    this.rows = 24;
    window.terminals.push(this);
    window.currentTerminal = this;
  }
  loadAddon() {}
  open() {}
  reset() {}
  write() {}
  dispose() {
    this.disposed = true;
  }
  onData(callback: (text: string) => void) {
    this.input = callback;
  }
}
