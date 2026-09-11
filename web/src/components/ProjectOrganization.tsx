import { Button, NativeSelect, TextInput } from "@mantine/core";
import { useRef, useState } from "react";
import { api, errorText } from "../api";
import type { Agent, Snapshot } from "../types";

export type Project = NonNullable<Snapshot["runtime"]["projects"]>[number];
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
    folder === "new" ? "" : folder?.name || project.name,
  );
  const [pending, setPending] = useState(false);
  const [error, setError] = useState("");
  const lock = useRef(false);
  const id = useRef(folder === "new" ? crypto.randomUUID() : folder?.id);
  const request = useRef<Record<string, unknown> | null>(null);
  const revision = useRef(project.organizationRevision || 0);
  return (
    <form
      onSubmit={async (event) => {
        event.preventDefault();
        if (lock.current) return;
        lock.current = true;
        setPending(true);
        setError("");
        request.current ||= {
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
        };
        try {
          await api("/api/projects", request.current);
          await saved();
        } catch (error) {
          setError(errorText(error));
        } finally {
          lock.current = false;
          setPending(false);
        }
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
        disabled={pending}
        onChange={(event) => {
          setName(event.currentTarget.value);
          request.current = null;
        }}
      />
      {error && <p role="alert">{error}</p>}
      <Button type="submit" mt="md" loading={pending} disabled={!name.trim()}>
        {folder === "new" ? "Create folder" : "Save name"}
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
  const [pending, setPending] = useState(false);
  const [error, setError] = useState("");
  const lock = useRef(false);
  const expected = useRef({
    folder: agent.projectFolder || null,
    revision: agent.projectFolderRevision || 0,
  });
  return (
    <form
      onSubmit={async (event) => {
        event.preventDefault();
        if (lock.current) return;
        lock.current = true;
        setPending(true);
        setError("");
        try {
          const result = await api("/api/organization", {
            id: agent.id,
            project_path: project.path,
            project_folder: folder || null,
            expected_folder: expected.current.folder,
            expected_revision: expected.current.revision,
          });
          if ((result.projectFolder || "") !== folder)
            throw new Error("Restart Studio to use project folders.");
          await saved();
        } catch (error) {
          setError(errorText(error));
        } finally {
          lock.current = false;
          setPending(false);
        }
      }}
    >
      <NativeSelect
        label="Move to folder"
        value={folder}
        disabled={pending}
        onChange={(event) => setFolder(event.currentTarget.value)}
        data={[
          { value: "", label: project.name },
          ...(project.folders || [])
            .map((item) => ({
              value: item.id,
              label: folderLabel(project.folders || [], item.id),
            }))
            .sort((a, b) => a.label.localeCompare(b.label)),
        ]}
      />
      {error && <p role="alert">{error}</p>}
      <Button type="submit" mt="md" loading={pending}>
        Move chat
      </Button>
    </form>
  );
}
