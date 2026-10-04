import { Button, NativeSelect, TextInput } from "@mantine/core";
import { useRef, useState } from "react";
import { useProjectSave } from "./useProjectSave";
import type { Agent, Snapshot } from "../types";

export type Project = NonNullable<Snapshot["runtime"]>["projects"][number];
export type ProjectFolder = NonNullable<Project["folders"]>[number];

export function folderLabel(folders: ProjectFolder[], id: string): string {
  const names: string[] = [];
  const seen = new Set<string>();
  let folder = folders.find((item) => item.id === id);
  while (folder && !seen.has(folder.id)) {
    seen.add(folder.id);
    names.unshift(folder.name);
    folder = folders.find((item) => item.id === folder!.parentId);
  }
  return names.join(" / ");
}

export function ProjectNameForm({
  project,
  folder,
  parentId,
  saved,
}: {
  project: Project;
  folder?: ProjectFolder | "new";
  parentId?: string;
  saved: () => Promise<void>;
}) {
  const [name, setName] = useState(
    folder === "new" ? "" : folder?.name || project.name || "",
  );
  const save = useProjectSave("/api/projects", saved);
  const id = useRef(folder === "new" ? crypto.randomUUID() : folder?.id);
  const revision = useRef(project.organizationRevision || 0);
  return (
    <form
      onSubmit={(event) => {
        event.preventDefault();
        if (typeof project.path !== "string") return;
        void save.submit({
          action:
            folder === "new"
              ? "add_folder"
              : folder
                ? "rename_folder"
                : "rename",
          path: project.path,
          name: name.trim(),
          folder_id: id.current,
          parent_id: parentId || null,
          expected_revision: revision.current,
        });
      }}
    >
      <p className="project-directory">{project.path}</p>
      {parentId && <p>In {folderLabel(project.folders || [], parentId)}</p>}
      <TextInput
        label={folder ? "Folder name" : "Project name"}
        value={name}
        autoFocus
        maxLength={255}
        required
        disabled={save.pending || save.frozen}
        onChange={(event) => {
          setName(event.currentTarget.value);
        }}
      />
      {save.error && <p role="alert">{save.error}</p>}
      <Button
        type="submit"
        mt="md"
        loading={save.pending}
        disabled={!name.trim()}
      >
        {save.retryLabel || (folder === "new" ? "Create folder" : "Save name")}
      </Button>
    </form>
  );
}

export function MoveChatForm({
  project,
  agent,
  saved,
}: {
  project: Project;
  agent: Agent;
  saved: () => Promise<void>;
}) {
  const [folder, setFolder] = useState(agent.projectFolder || "");
  const save = useProjectSave("/api/organization", saved, (result) => {
    if ((result.projectFolder || "") !== folder)
      throw new Error("Restart Studio to use project folders.");
  });
  const expected = useRef({
    folder: agent.projectFolder || null,
    revision: agent.projectFolderRevision || 0,
  });
  return (
    <form
      onSubmit={(event) => {
        event.preventDefault();
        if (typeof project.path !== "string") return;
        void save.submit({
          id: agent.id,
          project_path: project.path,
          project_folder: folder || null,
          expected_folder: expected.current.folder,
          expected_revision: expected.current.revision,
        });
      }}
    >
      <NativeSelect
        label="Move to folder"
        value={folder}
        disabled={save.pending || save.frozen}
        onChange={(event) => setFolder(event.currentTarget.value)}
        data={[
          { value: "", label: project.name || project.path || "Project" },
          ...(project.folders || [])
            .map((item) => ({
              value: item.id,
              label: folderLabel(project.folders || [], item.id),
            }))
            .sort((a, b) => a.label.localeCompare(b.label)),
        ]}
      />
      {save.error && <p role="alert">{save.error}</p>}
      <Button type="submit" mt="md" loading={save.pending}>
        {save.retryLabel || "Move chat"}
      </Button>
    </form>
  );
}

export function ConvertChatForm({
  project,
  source,
  target,
  scope,
  saved,
}: {
  project: Project;
  source: Agent;
  target: Agent;
  scope: string;
  saved: () => Promise<void>;
}) {
  const requestId = useRef(crypto.randomUUID());
  const save = useProjectSave(
    "/api/peer-teams",
    saved,
    undefined,
    `studio-peer-convert:${scope}:${source.id}:${target.id}`,
  );
  return (
    <form
      onSubmit={(event) => {
        event.preventDefault();
        void save.submit({
          action: "convert",
          path: project.path,
          member: source.id,
          target: target.id,
          expected_revision: project.peerTeamsRevision || 0,
          request_id: requestId.current,
        });
      }}
    >
      <p>
        Make <strong>{source.name}</strong> a subagent of{" "}
        <strong>{target.name}</strong>?
      </p>
      <ul>
        <li>
          The chat keeps its history, thread, account, model, and directory.
        </li>
        <li>Its workers and task board move to {target.name}.</li>
        <li>
          It leaves its peer team. A team with fewer than two chats dissolves.
        </li>
        <li>
          Saved peer messages stay available to you. Former peers lose agent
          access.
        </li>
        <li>Its previous broadcast history stays with its original workers.</li>
        <li>The chat pauses. {target.name} becomes its lead.</li>
      </ul>
      <p>
        Both agent trees must be idle. Active commands, pending input, workspace
        operations, and account transfers block the move.
      </p>
      {save.error && <p role="alert">{save.error}</p>}
      <Button type="submit" loading={save.pending}>
        {save.retryLabel || "Make subagent"}
      </Button>
    </form>
  );
}
