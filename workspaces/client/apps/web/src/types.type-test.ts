import type { PostBody } from "./api";
import { statusLabel, type Agent } from "./types";
import {
  defaultWorkspaceMode,
  type NewChatWorkspaceMode,
} from "./components/newChatSettings";

type AssertNever<T extends never> = T;

export type StatusInputStaysClosed = AssertNever<
  Exclude<Parameters<typeof statusLabel>[0], Agent["status"]>
>;
export type NewChatModeUsesRequestOwner = AssertNever<
  | Exclude<
      NewChatWorkspaceMode,
      NonNullable<PostBody<"/api/leads">["workspaceMode"]>
    >
  | Exclude<
      NonNullable<PostBody<"/api/leads">["workspaceMode"]>,
      NewChatWorkspaceMode
    >
>;

const acceptsAgentStatus: (status: Agent["status"]) => string = statusLabel;
const defaultMode: NewChatWorkspaceMode = defaultWorkspaceMode();
// @ts-expect-error Agent status must come from the generated closed domain.
statusLabel("unrecognized-status");
// @ts-expect-error Missing agent status is passed directly, without an empty-string sentinel.
statusLabel("");
// @ts-expect-error Shared folders are worker workspaces, not new-chat modes.
const invalidChatMode: NewChatWorkspaceMode = "shared";
const invalidWorkspace: Agent = {
  id: "invalid-workspace",
  // @ts-expect-error Agent workspace modes must come from the generated closed domain.
  workspaceMode: "unrecognized-workspace",
};

void acceptsAgentStatus;
void defaultMode;
void invalidChatMode;
void invalidWorkspace;
