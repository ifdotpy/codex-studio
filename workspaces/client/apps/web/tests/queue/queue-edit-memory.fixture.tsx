import React from "react";
import { createRoot } from "react-dom/client";
import MessageQueue from "../../src/components/MessageQueue";
import type { QueueItemDto } from "../../src/api";

export function mount() {
  const node = document.createElement("div");
  document.body.appendChild(node);
  const item = {
    id: "one",
    text: "original",
    created: 1,
    status: "pending",
    kind: "user",
  } as QueueItemDto;
  createRoot(node).render(
    <MessageQueue
      scope="test"
      items={[item]}
      canReorder={false}
      onEdit={async (_item, text) => {
        document.title = text;
      }}
      onCancel={async () => {}}
      onReorder={async () => {}}
    />,
  );
}
