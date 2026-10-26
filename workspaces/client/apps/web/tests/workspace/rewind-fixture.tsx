import type { Agent } from "../../src/types.ts";
import { required } from "../teams/helpers.ts";
import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import Restore from "../../src/components/ClaudeCheckpointRestore.tsx";
const agent: Agent = {
  id: "worker",
  provider: "claude",
  imageWorkspace: true,
  status: "paused",
};
function Fixture() {
  const [revision, setRevision] = useState(0);
  return (
    <MantineProvider>
      <Restore
        agent={agent}
        revision={revision}
        onRestored={() => setRevision((v) => v + 1)}
      />
    </MantineProvider>
  );
}
createRoot(required(document.getElementById("root"))).render(<Fixture />);
