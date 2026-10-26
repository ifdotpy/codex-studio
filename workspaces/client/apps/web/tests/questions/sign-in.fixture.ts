import type { components } from "../../src/generated/api.ts";

type Params = components["schemas"]["RequestParamsDto"];

export function signInCards(
  defaults: Pick<
    Params,
    "clientId" | "scope" | "linuxUser" | "source" | "expiresAt"
  >,
) {
  return (
    id: string,
    url: string,
    host: string,
    redirect: Params["redirect"] & components["schemas"]["JsonValue"],
    extra: Pick<Params, "userCode"> = {},
  ) =>
    ({
      id,
      agent: "worker",
      method: "browser/open",
      status: "pending",
      params: { bridgeId: id, url, host, redirect, ...defaults, ...extra },
    }) satisfies components["schemas"]["RequestEntityDto"];
}

export const browserSignInUrl =
  "https://accounts.example.com/oauth/authorize?client_id=kube&redirect_uri=http%3A%2F%2Flocalhost%3A8000&state=s";

export const phoneSignInUrl =
  "https://id.example.com/authorize?client_id=cli&redirect_uri=http%3A%2F%2F127.0.0.1%3A40645%2Foauth%2Fcallback&state=s1";
