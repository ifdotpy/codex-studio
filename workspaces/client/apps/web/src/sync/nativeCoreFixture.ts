/** A stand-in for the native core's `projection.*Rows` commands (`projections.rs`).
 * It keeps rows in memory, orders them by id and pages replies like the core. */
export function createNativeCoreFixture(pageRows = Infinity) {
  const rows = new Map<string, Record<string, unknown>>();
  const calls: { method: string; params: Record<string, unknown> }[] = [];
  const scope = (params: Record<string, unknown>, id = "") =>
    `${params.workspace}|${params.prefix}/${id}`;
  async function request<T>(method: string, params: unknown): Promise<T> {
    const input = params as Record<string, unknown>;
    calls.push({ method, params: input });
    switch (method) {
      case "projection.getRows":
        return (input.ids as string[]).flatMap((id) => {
          const row = rows.get(scope(input, id));
          return row ? [structuredClone(row)] : [];
        }) as T;
      case "projection.rangeRows": {
        const lower = scope(input, (input.gte ?? input.gt) as string);
        const upper = scope(input, input.lt as string);
        const found = [...rows.keys()]
          .filter(
            (key) =>
              (input.gte === undefined ? key > lower : key >= lower) &&
              key < upper,
          )
          .sort();
        const limit = Math.min(pageRows, (input.limit as number) ?? Infinity);
        return {
          rows: found
            .slice(0, limit)
            .map((key) => structuredClone(rows.get(key))),
          more: found.length > limit,
        } as T;
      }
      case "projection.putRows":
        for (const row of input.rows as Record<string, unknown>[])
          rows.set(scope(input, row.id as string), structuredClone(row));
        return { written: (input.rows as unknown[]).length } as T;
      case "projection.pruneRows": {
        const keep = `${input.workspace}|${input.prefix}/`;
        for (const key of rows.keys())
          if (key.startsWith(`${input.workspace}|`) && !key.startsWith(keep))
            rows.delete(key);
        return { removed: 0 } as T;
      }
      default:
        throw new Error(`Unexpected native command ${method}`);
    }
  }
  return { request, rows, calls };
}
