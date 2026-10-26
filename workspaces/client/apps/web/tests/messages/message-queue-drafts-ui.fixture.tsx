import { required } from "./browser-helpers.ts";
import type { QueueItem } from "../../src/components/MessageQueue.tsx";

import { useState } from "react";
import { createRoot } from "react-dom/client";
import MessageQueue from "../../src/components/MessageQueue.tsx";
function App() {
  const [items, setItems] = useState<QueueItem[]>([
    {
      id: "q1",
      text: "Original message",
      created: 1,
      status: "pending",
      kind: "user",
    },
  ]);
  return (
    <MessageQueue
      scope="journal-fixture"
      items={items}
      canReorder={true}
      onEdit={async (item, text) => setItems([{ ...item, text }])}
      onCancel={async () => setItems([])}
      onReorder={async () => {}}
    />
  );
}
createRoot(required(document.getElementById("root"))).render(<App />);
