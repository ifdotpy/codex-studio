import type { components } from "../../src/generated/api.ts";

/** Empty radio state for sidebar fixtures that only render the shared chat row. */
export const sidebarFixtureRadio = {
  direct: true,
  active: null,
  error: null,
  next: [],
  revision: 0,
  speaker: null,
  status: "idle",
  teamId: "",
} satisfies components["schemas"]["RoomRadio"];
