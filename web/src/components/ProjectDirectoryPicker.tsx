import { ActionIcon, Button, TextInput, UnstyledButton } from "@mantine/core";
import { ArrowUp, ChevronRight, Folder, Search } from "lucide-react";
import { useEffect, useState } from "react";
import { api, errorText } from "../api";
import "./project-directory-picker.css";

type Directory = {
  path: string;
  parent?: string;
  directories: { name: string; path: string }[];
};

export default function ProjectDirectoryPicker({
  initialPath,
  onSelect,
}: {
  initialPath?: string;
  onSelect: (path: string) => Promise<void>;
}) {
  const [path, setPath] = useState(initialPath);
  const [typedPath, setTypedPath] = useState(initialPath || "");
  const [directory, setDirectory] = useState<Directory | null>(null);
  const [query, setQuery] = useState("");
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");
  const [selectionError, setSelectionError] = useState("");
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    let active = true;
    setLoading(true);
    setError("");
    api<Directory>(
      "/api/directories" + (path ? "?path=" + encodeURIComponent(path) : ""),
    )
      .then((result) => {
        if (active) setDirectory(result);
      })
      .catch((failure) => {
        if (active) setError(errorText(failure));
      })
      .finally(() => {
        if (active) setLoading(false);
      });
    return () => {
      active = false;
    };
  }, [path, attempt]);

  const navigate = (next: string) => {
    setLoading(true);
    setSelectionError("");
    setQuery("");
    if (next === path) setAttempt((value) => value + 1);
    setPath(next);
    setTypedPath(next);
  };
  const rows = directory?.directories.filter((row) =>
    row.name.toLocaleLowerCase().includes(query.toLocaleLowerCase()),
  );
  return (
    <div className="directory-picker" aria-busy={loading || saving}>
      <form
        className="directory-path-entry"
        onSubmit={(event) => {
          event.preventDefault();
          if (typedPath.trim()) navigate(typedPath.trim());
        }}
      >
        <TextInput
          label="Folder path"
          placeholder="/Users/…/Projects/…"
          value={typedPath}
          onChange={(event) => setTypedPath(event.currentTarget.value)}
          disabled={saving}
        />
        <Button type="submit" disabled={saving || !typedPath.trim()}>
          Go
        </Button>
      </form>
      {window.codexDesktop && (
        <Button
          variant="light"
          onClick={() => {
            void window
              .codexDesktop!.pickDirectory()
              .then((folder) => {
                if (folder) navigate(folder);
              })
              .catch((failure) => setSelectionError(errorText(failure)));
          }}
          disabled={saving}
        >
          Browse in Finder…
        </Button>
      )}
      <div className="directory-location">
        <ActionIcon
          aria-label="Parent folder"
          disabled={loading || saving || !directory?.parent}
          onClick={() => directory?.parent && navigate(directory.parent)}
        >
          <ArrowUp size={18} />
        </ActionIcon>
        <span>{loading || error ? path || "Projects" : directory?.path}</span>
      </div>
      <TextInput
        aria-label="Filter folders"
        placeholder="Filter folders"
        leftSection={<Search size={16} />}
        value={query}
        disabled={loading || saving}
        onChange={(event) => setQuery(event.currentTarget.value)}
      />
      <div className="directory-list" aria-label="Folders">
        {loading ? (
          <p role="status">Loading folders…</p>
        ) : error ? (
          <div role="alert">
            <p>{error}</p>
            <Button onClick={() => setAttempt((value) => value + 1)}>
              Retry
            </Button>
            {directory && directory.path !== path && (
              <Button onClick={() => navigate(directory.path)}>
                Back to previous folder
              </Button>
            )}
          </div>
        ) : rows?.length ? (
          rows.map((row) => (
            <UnstyledButton
              className="directory-row"
              key={row.path}
              disabled={saving}
              onClick={() => navigate(row.path)}
            >
              <Folder size={17} />
              <span>{row.name}</span>
              <ChevronRight size={15} />
            </UnstyledButton>
          ))
        ) : (
          <p>{query ? "No matching folders" : "No folders inside"}</p>
        )}
      </div>
      {selectionError && (
        <p className="directory-error" role="alert">
          {selectionError}
        </p>
      )}
      <div className="directory-footer">
        <Button
          variant="filled"
          loading={saving}
          disabled={loading || !!error || !directory}
          onClick={async () => {
            if (!directory || saving) return;
            setSaving(true);
            setSelectionError("");
            try {
              await onSelect(directory.path);
            } catch (failure) {
              setSelectionError(errorText(failure));
            } finally {
              setSaving(false);
            }
          }}
        >
          Use this folder
        </Button>
      </div>
    </div>
  );
}
