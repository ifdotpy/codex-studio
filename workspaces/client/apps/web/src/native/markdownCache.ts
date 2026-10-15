// Compact JSON trees: bare numbers reference interned strings; arrays are
// tagged as object (0, key/value pairs), array (1), or number (2). This keeps
// repeated field names and sentence text once without changing the tree API.
type Packed = number | boolean | null | Packed[];
function pack(value: unknown, intern: (text: string) => number): Packed {
  if (typeof value === "string") return intern(value);
  if (typeof value === "number") return [2, value];
  if (value === null || typeof value === "boolean") return value;
  if (Array.isArray(value))
    return [1, ...value.map((item) => pack(item, intern))];
  if (typeof value === "object") {
    return [
      0,
      ...Object.entries(value).flatMap(([key, item]) => [
        intern(key),
        pack(item, intern),
      ]),
    ];
  }
  throw new TypeError("Markdown cache requires JSON values.");
}
function unpack(value: Packed, strings: string[]): unknown {
  if (typeof value === "number") return strings[value];
  if (!Array.isArray(value)) return value;
  if (value[0] === 2) return value[1];
  if (value[0] === 1)
    return value.slice(1).map((item) => unpack(item, strings));
  const entries: [string, unknown][] = [];
  for (let index = 1; index < value.length; index += 2) {
    entries.push([
      strings[value[index] as number],
      unpack(value[index + 1], strings),
    ]);
  }
  return Object.fromEntries(entries);
}

// Cache serialized inert trees, not React nodes or workspace-dependent elements.
// UTF-16 source/output plus bookkeeping is an accounting bound, not heap size.
export class MarkdownCache {
  private entries = new Map<string, { json: string; bytes: number }>();
  private bytes = 0;
  constructor(
    private maxBytes = 4 * 1024 * 1024,
    private maxEntries = 512,
  ) {}
  get(text: string): unknown | undefined {
    const entry = this.entries.get(text);
    if (!entry) return undefined;
    this.entries.delete(text);
    this.entries.set(text, entry);
    const { tree, strings } = JSON.parse(entry.json);
    return unpack(tree, strings);
  }
  set(text: string, value: unknown) {
    const strings: string[] = [];
    const indexes = new Map<string, number>();
    const intern = (value: string): number => {
      let index = indexes.get(value);
      if (index === undefined) {
        index = strings.length;
        strings.push(value);
        indexes.set(value, index);
      }
      return index;
    };
    const tree = pack(value, intern);
    const json = JSON.stringify({ tree, strings });
    const bytes = 128 + 2 * (text.length + json.length);
    const old = this.entries.get(text);
    if (old) {
      this.bytes -= old.bytes;
      this.entries.delete(text);
    }
    if (bytes > this.maxBytes || this.maxEntries < 1) return;
    this.entries.set(text, { json, bytes });
    this.bytes += bytes;
    while (this.bytes > this.maxBytes || this.entries.size > this.maxEntries) {
      const oldest = this.entries.keys().next().value!;
      this.bytes -= this.entries.get(oldest)!.bytes;
      this.entries.delete(oldest);
    }
  }
}
export const completedMarkdown = new MarkdownCache();
