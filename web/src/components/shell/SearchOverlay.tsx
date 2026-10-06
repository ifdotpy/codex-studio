import {
  Badge,
  Button,
  Loader,
  Modal,
  TextInput,
  UnstyledButton,
} from "@mantine/core";
import { Search } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { get, errorText, type GetResult } from "../../api";
import type { Snapshot } from "../../types";
import { useWorkspaceResource as useResource } from "../useWorkspaceResource";
import "./Workspace.css";
type SearchProps = {
  data: Snapshot;
  revision: number;
  onSelect: (id: string, messageId?: string) => void;
  onClose: () => void;
  navigate: (section: string, agent?: string) => void;
};
export default function SearchOverlay(
  props: Omit<SearchProps, "revision"> & { opened: boolean },
) {
  return (
    <Modal
      opened={props.opened}
      onClose={props.onClose}
      title="Search messages"
      size="lg"
    >
      <p className="workspace-muted">
        All messages and archived chats. Ctrl+K / ⌘K
      </p>
      {props.opened && <SearchContent {...props} revision={0} />}
    </Modal>
  );
}
export function SearchContent(c: SearchProps) {
  const sourceRequest = useRef(0);
  useEffect(
    () => () => {
      sourceRequest.current++;
    },
    [],
  );
  type SearchResult = GetResult<"/api/search">["results"][number];
  type SearchItem = GetResult<"/api/search/item">;
  type SearchSource =
    | (SearchResult & { loading: boolean })
    | (SearchItem & { loading: boolean });
  const [source, setSource] = useState<SearchSource | null>(null),
    [sourceError, setSourceError] = useState("");
  const [query, setQuery] = useState(""),
    [search, setSearch] = useState("");
  const state = useResource(search ? "/api/search" : null, c.revision, {
    query: { q: search },
  });
  return (
    <>
      <form
        className="workspace-toolbar"
        onSubmit={(e) => {
          e.preventDefault();
          setSearch(query.trim());
        }}
      >
        <TextInput
          className="workspace-grow"
          autoFocus
          aria-label="Search all conversations"
          placeholder="Search messages, work, and agent chats"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          leftSection={<Search size={16} />}
        />
        <Button
          variant="filled"
          type="submit"
          disabled={!query.trim()}
          loading={state.loading}
        >
          Search
        </Button>
      </form>
      {state.error ? (
        <p role="alert" className="workspace-error">
          {state.error}
        </p>
      ) : state.loading && !state.data ? (
        <Loader size="sm" />
      ) : null}
      {(state.data?.results || []).map((result, index) => (
        <UnstyledButton
          className="workspace-row"
          key={`${result.kind}:${result.id}:${index}`}
          onClick={async () => {
            const request = ++sourceRequest.current;
            setSource({ ...result, loading: true });
            setSourceError("");
            try {
              const record = await get("/api/search/item", {
                query: { id: result.id },
              });
              if (request === sourceRequest.current)
                setSource({ ...result, ...record, loading: false });
            } catch (e) {
              if (request === sourceRequest.current) {
                setSourceError(errorText(e));
                setSource({ ...result, loading: false });
              }
            }
          }}
        >
          <div className="workspace-row-head">
            <Badge size="xs" variant="light" color="gray">
              {result.kind}
            </Badge>
            <small>
              {c.data.threads.find((a) => a.id === result.agent)?.name ||
                result.agent ||
                "Unassigned"}
            </small>
          </div>
          <p className="workspace-prose">{result.text}</p>
        </UnstyledButton>
      ))}
      {search && state.data && !state.data.results?.length && (
        <div className="workspace-empty">No results for “{search}”.</div>
      )}
      {!search && (
        <div className="workspace-empty">
          Searches all messages, archived chats too.
        </div>
      )}
      <Modal
        opened={!!source}
        onClose={() => {
          sourceRequest.current++;
          setSource(null);
        }}
        title="Search source"
        size="lg"
      >
        {source && (
          <>
            <div className="workspace-toolbar">
              <Badge variant="light" color="gray">
                {source.kind || ("type" in source ? source.type : "")}
              </Badge>
              <small className="workspace-muted">{source.id}</small>
            </div>
            {sourceError && (
              <p role="alert" className="workspace-error">
                {sourceError}
              </p>
            )}
            {source.loading ? (
              <Loader size="sm" />
            ) : (
              <p className="workspace-prose">
                {typeof source.text === "string"
                  ? source.text
                  : JSON.stringify(source, null, 2)}
              </p>
            )}
            <Button
              variant="light"
              onClick={() => {
                if (source.kind === "plan") c.navigate("plan", source.agent);
                else {
                  c.onSelect(source.room || source.agent, source.id);
                  c.onClose();
                }
                setSource(null);
              }}
            >
              Open {source.kind === "plan" ? "plan" : "chat"}
            </Button>
          </>
        )}
      </Modal>
    </>
  );
}
