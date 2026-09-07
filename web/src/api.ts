let token = "";
let workspace = "";
export function setWorkspace(value: string) {
  workspace = value;
}
export class ApiError extends Error {
  constructor(
    message: string,
    public status: number,
  ) {
    super(message);
  }
}
export function setToken(value: string) {
  token = value;
}
export async function api<T = any>(path: string, body?: unknown): Promise<T> {
  const response = await fetch(
    path,
    body === undefined
      ? {}
      : {
          method: "POST",
          headers: {
            "Content-Type": "application/json",
            "X-Canvas-Token": token,
            ...(workspace ? { "X-Canvas-Workspace": workspace } : {}),
          },
          body: JSON.stringify(body),
        },
  );
  const data = await response.json();
  if (!response.ok)
    throw new ApiError(
      data.error || `Request failed (${response.status})`,
      response.status,
    );
  return data;
}
export const errorText = (e: unknown) =>
  e instanceof Error ? e.message : String(e);
export function saved<T>(key: string, fallback: T): T {
  try {
    return JSON.parse(localStorage.getItem(key) || "null") ?? fallback;
  } catch {
    return fallback;
  }
}
export function save(key: string, value: unknown) {
  try {
    localStorage.setItem(key, JSON.stringify(value));
  } catch {
    /* Server data remains available if browser storage is full. */
  }
}
