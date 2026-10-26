export type CompiledMarkdownBlock =
  | { readonly html: string; readonly kind?: never; readonly source?: never }
  | {
      readonly kind: "html" | "mermaid";
      readonly source: string;
      readonly html?: never;
    };

// Retain only completed source and compiled strings, never tokens or React nodes.
// Charge UTF-16 source/output plus bookkeeping per entry and block.
// These are cache accounting limits, not a measurement of the JavaScript heap.
export class CompletedMarkdownCache {
  private entries = new Map<
    string,
    { blocks: readonly CompiledMarkdownBlock[]; bytes: number }
  >();
  private bytes = 0;

  private readonly maxBytes: number;
  private readonly maxEntries: number;
  constructor(maxBytes = 4 * 1024 * 1024, maxEntries = 512) {
    this.maxBytes = maxBytes;
    this.maxEntries = maxEntries;
  }

  get(text: string) {
    const entry = this.entries.get(text);
    if (!entry) return undefined;
    this.entries.delete(text);
    this.entries.set(text, entry);
    return entry.blocks;
  }

  set(text: string, blocks: readonly CompiledMarkdownBlock[]) {
    const bytes =
      128 +
      blocks.length * 64 +
      2 *
        (text.length +
          blocks.reduce(
            (total, block) => total + (block.html ?? block.source).length,
            0,
          ));
    const previous = this.entries.get(text);
    if (previous) {
      this.bytes -= previous.bytes;
      this.entries.delete(text);
    }
    if (bytes > this.maxBytes || this.maxEntries < 1) return;
    this.entries.set(text, { blocks, bytes });
    this.bytes += bytes;
    while (this.bytes > this.maxBytes || this.entries.size > this.maxEntries) {
      const oldest = this.entries.keys().next().value!;
      this.bytes -= this.entries.get(oldest)!.bytes;
      this.entries.delete(oldest);
    }
  }
}
