import type { Agent } from "../../src/types.ts";
import type { ChatReadState } from "../../src/components/useChatReadState.ts";
import type { PostBody } from "../../src/apiTypes.ts";
import { fixturePostBody } from "./http.ts";
export type ReadWrite = PostBody<"/api/organization"> & {
  read_state: NonNullable<PostBody<"/api/organization">["read_state"]>;
};
export function readWrite(body: unknown): ReadWrite {
  const value = fixturePostBody("/api/organization", body);
  if (!value.read_state) throw new Error("Missing read state");
  return { ...value, read_state: value.read_state };
}
// This fixture alone writes and reads the stored read-state document.
export function fixtureReadState(text: string): ChatReadState | null {
  return JSON.parse(text) as ChatReadState | null;
}
export const initialAgent: Agent = {
  id: "one",
  threadId: "thread",
  lastCompletedTurn: "turn",
  lastCompletedTurnStatus: "completed",
  source: "managed",
  status: "completed",
  name: "One",
  model: "sol",
  created: 1,
  readStateSupported: true,
};
