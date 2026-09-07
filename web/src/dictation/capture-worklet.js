class StudioCapture extends AudioWorkletProcessor {
  constructor() {
    super();
    this.buffer = new Int16Array(4096);
    this.offset = 0;
    this.stopped = false;
    this.port.onmessage = (event) => {
      if (event.data === "stop") {
        this.flush();
        this.stopped = true;
        this.port.postMessage("stopped");
      }
    };
  }
  flush() {
    if (this.offset) {
      const pcm = this.buffer.slice(0, this.offset);
      this.port.postMessage(pcm, [pcm.buffer]);
      this.offset = 0;
    }
  }
  process(inputs) {
    if (this.stopped) return false;
    for (const sample of inputs[0]?.[0] || []) {
      this.buffer[this.offset++] = Math.round(
        Math.max(-1, Math.min(1, sample)) * 32767,
      );
      if (this.offset === this.buffer.length) this.flush();
    }
    return true;
  }
}
registerProcessor("studio-capture", StudioCapture);
