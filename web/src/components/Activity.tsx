import { Wrench } from "lucide-react";
import type { Message } from "../types";

function label(item: Message) {
  const titles: Record<string, string> = {
    commandExecution: "Run command",
    dynamicToolCall: "Tool call",
    webSearch: "Search the web",
    fileChange: "File changes",
    contextCompaction: "Compact context",
    "turn/plan/updated": "Plan",
    "turn/diff/updated": "Changes",
  };
  return titles[item.title || ""] || item.title || "Tool activity";
}

export default function Activity({ items }: { items: Message[] }) {
  const content = (item: Message) => (
    <>
      <pre>{item.text}</pre>
      {item.truncated && <p className="notice">This activity is clipped.</p>}
    </>
  );
  return (
    <details className="tool-group">
      <summary>
        <Wrench size={13} />
        <span>
          {items.length === 1 ? label(items[0]) : `${items.length} actions`}
        </span>
      </summary>
      {items.length === 1 ? (
        content(items[0])
      ) : (
        <div className="activity-list">
          {items.map((item) => (
            <details key={item.id}>
              <summary>{label(item)}</summary>
              {content(item)}
            </details>
          ))}
        </div>
      )}
    </details>
  );
}
