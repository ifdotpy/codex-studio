type Guard = () => Promise<void>;
const guards = new Set<Guard>();

export function guardNativeClose(guard: Guard) {
  guards.add(guard);
  return () => {
    guards.delete(guard);
  };
}

export async function prepareNativeClose() {
  await Promise.all([...guards].map((guard) => guard()));
}
