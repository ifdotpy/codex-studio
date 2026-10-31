export type ChatRoute = { id: string; kind: "agent" | "room" | "legacy" };

export function readChatRoute(search: string): ChatRoute | null {
  const params = new URLSearchParams(search);
  const id = params.get("chat");
  if (!id) return null;
  const kind = params.get("chatKind");
  return { id, kind: kind === "room" || kind === "legacy" ? kind : "agent" };
}

export function writeChatRoute(route: ChatRoute | null) {
  const url = new URL(location.href);
  if (route) {
    url.searchParams.set("chat", route.id);
    if (route.kind === "agent") url.searchParams.delete("chatKind");
    else url.searchParams.set("chatKind", route.kind);
  } else {
    url.searchParams.delete("chat");
    url.searchParams.delete("chatKind");
  }
  history.pushState(null, "", url);
}
