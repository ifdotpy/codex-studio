import { isRemoteServerView } from "../servers/environment";
import {
  ActionIcon,
  Button,
  Select,
  TextInput,
  UnstyledButton,
} from "@mantine/core";
import { ArrowUp, ChevronRight, Folder, Search } from "lucide-react";
import { useEffect, useState } from "react";
import { get, errorText } from "../api";
import type { paths } from "../generated/api";
import {
  readProjectServers,
  type ProjectServerChoice,
} from "../servers/projectLocations";
import "./project-directory-picker.css";

type Directory =
  paths["/api/directories"]["get"]["responses"][200]["content"]["application/json"];

export default function ProjectDirectoryPicker({
  initialPath,
  onSelect,
  serverChoices,
  initialServer,
  projectId,
  showServerSelector = true,
  submitLabel = "Add project",
}: {
  initialPath?: string;
  onSelect: (path: string, server: string) => Promise<void>;
  serverChoices?: ProjectServerChoice[];
  initialServer?: string;
  projectId?: string;
  showServerSelector?: boolean;
  submitLabel?: string;
}) {
  const [servers, setServers] = useState<ProjectServerChoice[]>(
    serverChoices || [{ id: "local", label: "This computer" }],
  );
  const [server, setServer] = useState(
    initialServer || serverChoices?.[0]?.id || "local",
  );
  const [serverError, setServerError] = useState("");
  const [matches, setMatches] = useState<string[]>([]);
  const selectedServer = servers.find((row) => row.id === server);
  useEffect(() => {
    if (serverChoices) {
      setServers(serverChoices);
      return;
    }
    let active = true;
    void readProjectServers()
      .then((rows) => {
        if (active) setServers(rows);
      })
      .catch((error) => {
        if (active) setServerError(errorText(error));
      });
    return () => {
      active = false;
    };
  }, [serverChoices]);
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
    get("/api/directories", {
      query: {
        ...(path ? { path } : {}),
        ...(server !== "local" ? { server } : {}),
      },
    })
      .then((result) => {
        if (active) {
          setDirectory(result);
          setTypedPath((current) =>
            current.trim() === (path || "") ? result.path : current,
          );
        }
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
  }, [path, attempt, server]);

  useEffect(() => {
    setMatches([]);
    if (!projectId) return;
    let active = true;
    void get("/api/project-locations", {
      query: {
        project: projectId,
        server,
        action: "matches",
        ...(directory ? { path: directory.path } : {}),
      },
    })
      .then((result) => {
        if (active) setMatches((result.matches || []).map((row) => row.path));
      })
      .catch(() => {});
    return () => {
      active = false;
    };
  }, [projectId, server, directory?.path]);

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
  const pathChanged = !!directory && typedPath.trim() !== directory.path;
  return (
    <div
      className="directory-picker"
      aria-busy={loading || saving}
      data-server-selector={showServerSelector}
    >
      {showServerSelector && (
        <Select
          label="Server"
          value={server}
          disabled={saving}
          data={servers.map((row) => ({
            value: row.id,
            label: row.label + (row.disabled ? " (Offline)" : ""),
            disabled: row.disabled,
          }))}
          onChange={(next) => {
            if (!next || next === server) return;
            setServer(next);
            setPath(undefined);
            setTypedPath("");
            setDirectory(null);
            setQuery("");
            setSelectionError("");
          }}
        />
      )}
      {serverError && <p role="alert">{serverError}</p>}
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
          Open path
        </Button>
      </form>
      {window.codexDesktop && !isRemoteServerView && server === "local" && (
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
              <span>
                {row.name}
                {matches.includes(row.path) && (
                  <small> (same git origin)</small>
                )}
              </span>
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
        {!loading && !error && pathChanged && (
          <p role="status">Select Open path to open the entered folder.</p>
        )}
        <Button
          variant="filled"
          loading={saving}
          disabled={
            loading ||
            !!error ||
            !directory ||
            pathChanged ||
            !!selectedServer?.disabled
          }
          onClick={async () => {
            if (!directory || saving) return;
            setSaving(true);
            setSelectionError("");
            try {
              await onSelect(directory.path, server);
            } catch (failure) {
              setSelectionError(errorText(failure));
            } finally {
              setSaving(false);
            }
          }}
        >
          {submitLabel}
        </Button>
      </div>
    </div>
  );
}
