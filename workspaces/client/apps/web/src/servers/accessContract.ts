import type { components, paths } from "../generated/api";

export type ServerAccessState = components["schemas"]["AccessSnapshot"];
export type ServerIdentityAlert = components["schemas"]["IdentityAlert"];
type ManagementBody =
  paths["/api/multi-server"]["post"]["requestBody"]["content"]["application/json"];
// The UI only issues these management actions and always supplies a request ID.
export type ServerAccessRequest =
  | Extract<
      ManagementBody,
      {
        action:
          | "revoke"
          | "unrevoke"
          | "discover"
          | "ui_invite"
          | "name"
          | "alias";
      }
    >
  | (Omit<Extract<ManagementBody, { action: "create_invite" }>, "label"> & {
      label?: string;
    })
  | (Omit<
      Extract<ManagementBody, { action: "settings" }>,
      "requestId" | "autoPair" | "autoUpdate"
    > & { requestId: string } & (
        | { autoPair: boolean; autoUpdate?: boolean }
        | { autoPair?: boolean; autoUpdate: boolean }
      ));
