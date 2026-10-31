export type ScreenRoute = "tasks" | "background" | "usage";

export function writeScreenRoute(screen: ScreenRoute, agentId: string | null) {
  const url = new URL(location.href);
  if (agentId) url.searchParams.set(screen, agentId);
  else url.searchParams.delete(screen);
  history.pushState(null, "", url);
}
