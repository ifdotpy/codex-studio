import type { AgentEntityDto as Agent } from "../../src/generated/api.ts";
import type { ChatReadState } from "../../src/components/useChatReadState.ts";
import type { PostBody } from "../../src/apiTypes.ts";
export type ReadWrite = PostBody<"/api/organization"> & {
  read_state: NonNullable<PostBody<"/api/organization">["read_state"]>;
};
export function readWrite(body: unknown): ReadWrite {
  // The renderer under test sends this generated organization request with read_state.
  return body as ReadWrite;
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
