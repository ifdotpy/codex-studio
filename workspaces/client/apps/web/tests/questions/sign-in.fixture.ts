declare global {
  interface Window {
    __notes: string[];
  }
}
import { approval } from "../../src/approvalFixtures.ts";
import type { Approval } from "../../src/approvals.ts";

type SignInFields = {
  clientId?: string;
  scope?: string;
  linuxUser?: string;
  source?: string;
  expiresAt?: number;
  userCode?: string;
};

export function signInCards(
  defaults: Pick<
    SignInFields,
    "clientId" | "scope" | "linuxUser" | "source" | "expiresAt"
  >,
) {
  return (
    id: string,
    url: string,
    host: string,
    redirect: { host: string; port: number; path: string } | null,
    extra: Pick<SignInFields, "userCode"> = {},
  ): Approval =>
    approval({
      id,
      agent: "worker",
      request: { kind: "browser_open", url },
      params: { bridgeId: id, host, redirect, ...defaults, ...extra },
    });
}

export const browserSignInUrl =
  "https://accounts.example.com/oauth/authorize?client_id=kube&redirect_uri=http%3A%2F%2Flocalhost%3A8000&state=s";

export const phoneSignInUrl =
  "https://id.example.com/authorize?client_id=cli&redirect_uri=http%3A%2F%2F127.0.0.1%3A40645%2Foauth%2Fcallback&state=s1";
