import type { ITerminalOptions } from "@xterm/xterm";
import type { Root } from "react-dom/client";
import type { mount } from "./terminal-input-fixture.ts";
declare global {
  interface Window {
    terminalInputRecoveryTerminals: Terminal[];
    terminalInputRecoveryCurrentTerminal: Terminal;
    terminalInputRecoveryRetiredTerminal: Terminal;
    terminalInputRecoveryRoot: Root | null;
    terminalInputRecoveryNotices: string[];
    terminalInputRecoveryAdvance: number;
    terminalInputRecoveryMount: typeof mount;
  }
}
export class Terminal {
  options: ITerminalOptions;
  cols = 100;
  rows = 24;
  disposed = false;
  input: ((text: string) => void) | undefined;
  constructor(options: ITerminalOptions) {
    this.options = options;
    window.terminalInputRecoveryTerminals.push(this);
    window.terminalInputRecoveryCurrentTerminal = this;
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
