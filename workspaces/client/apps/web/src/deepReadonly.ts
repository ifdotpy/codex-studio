/** A value whose fields and arrays are read-only at every depth. */
export type DeepReadonly<T> = T extends (infer R)[]
  ? DeepReadonlyArray<R>
  : T extends (...args: never[]) => unknown
    ? T
    : T extends object
      ? DeepReadonlyObject<T>
      : T;
interface DeepReadonlyArray<T> extends ReadonlyArray<DeepReadonly<T>> {}
type DeepReadonlyObject<T> = { readonly [P in keyof T]: DeepReadonly<T[P]> };
