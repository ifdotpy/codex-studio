import { ActionIcon, Button, Popover } from "@mantine/core";
import { Files } from "lucide-react";
import { useState } from "react";
import type { DraftVersion } from "../sync/drafts";

export default function DraftVersions(p: {
  versions: DraftVersion[];
  useVersion: (version: DraftVersion, mode: "replace" | "append") => void;
  dismiss: (version: DraftVersion) => void;
}) {
  const [opened, setOpened] = useState(false);
  if (!p.versions.length) return null;
  return (
    <Popover
      opened={opened}
      onChange={setOpened}
      position="top-start"
      width={320}
      withinPortal
    >
      <Popover.Target>
        <ActionIcon
          type="button"
          variant="subtle"
          aria-label={`Other drafts (${p.versions.length})`}
          title="Other drafts"
          onClick={() => setOpened(!opened)}
        >
          <Files size={18} />
        </ActionIcon>
      </Popover.Target>
      <Popover.Dropdown className="draft-versions">
        <strong>Other drafts</strong>
        <p>Replace the message text or add a draft to it.</p>
        {p.versions.map((version) => (
          <div
            className="draft-version"
            key={JSON.stringify([version.id, version.text])}
          >
            <small>
              {version.id.endsWith(":conflict")
                ? "Saved alternative"
                : Number.isFinite(version.updated) && version.updated > 0
                  ? new Date(version.updated).toLocaleString()
                  : "Time unavailable"}
              {version.device && !version.id.endsWith(":conflict") && (
                <span style={{ display: "block", overflowWrap: "anywhere" }}>
                  Device ID: {version.device}
                </span>
              )}
            </small>
            <pre>{version.text}</pre>
            <div className="draft-version-actions">
              <Button
                type="button"
                size="xs"
                onClick={() => {
                  p.useVersion(version, "replace");
                  setOpened(false);
                }}
              >
                Replace text
              </Button>
              <Button
                type="button"
                size="xs"
                variant="light"
                onClick={() => {
                  p.useVersion(version, "append");
                  setOpened(false);
                }}
              >
                Add to text
              </Button>
              <Button
                type="button"
                size="xs"
                variant="subtle"
                onClick={() => p.dismiss(version)}
              >
                Dismiss
              </Button>
            </div>
          </div>
        ))}
        <Button
          type="button"
          size="xs"
          variant="subtle"
          onClick={() => {
            for (const version of p.versions) p.dismiss(version);
            setOpened(false);
          }}
        >
          Dismiss all
        </Button>
      </Popover.Dropdown>
    </Popover>
  );
}
