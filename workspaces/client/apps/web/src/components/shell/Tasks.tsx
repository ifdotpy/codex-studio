import {
  Badge,
  Button,
  NativeSelect,
  Textarea,
  TextInput,
} from "@mantine/core";
import { useRef, useState } from "react";
import { errorText, type PostBody } from "../../api";
import {
  useServerApi,
  useServerStorage,
} from "../../servers/ServerServicesContext";
import { createMutationJournal } from "../../servers/mutationIdentity";
import { useWorkspaceResource } from "../useWorkspaceResource";
import {
  gateFields,
  parseGate,
  parseResultSchema,
  prettyJson,
  requiresGateOverride,
  taskStatus,
  type Gate,
  type ReviewTask,
} from "./taskReviewModel";
import "./Tasks.css";

type WorkBody = PostBody<"/api/work">;
type Write = (body: WorkBody) => Promise<boolean>;

function JsonView({ value, label }: { value: unknown; label: string }) {
  return (
    <pre aria-label={label} className="task-review-json">
      {prettyJson(value)}
    </pre>
  );
}

function GateEditor({
  command,
  timeout,
  onCommand,
  onTimeout,
}: {
  command: string;
  timeout: string;
  onCommand: (value: string) => void;
  onTimeout: (value: string) => void;
}) {
  return (
    <>
      <Textarea
        label="Gate command"
        description="Leave empty to disable the gate."
        value={command}
        onChange={(e) => onCommand(e.currentTarget.value)}
        autosize
        minRows={2}
      />
      <TextInput
        label="Gate timeout (milliseconds)"
        inputMode="numeric"
        disabled={!command.trim()}
        value={timeout}
        onChange={(e) => onTimeout(e.currentTarget.value)}
      />
    </>
  );
}

function DefaultGate({
  gate,
  write,
  busy,
}: {
  gate: Gate;
  write: Write;
  busy: boolean;
}) {
  const initial = gateFields(gate);
  const [command, setCommand] = useState(initial.command);
  const [timeout, setTimeout] = useState(initial.timeout);
  const [error, setError] = useState("");
  return (
    <details className="task-review-card">
      <summary>Team default gate</summary>
      <p>The default applies to new tasks. Existing tasks keep their gate.</p>
      {gate ? (
        <JsonView label="Current team default gate" value={gate} />
      ) : (
        <p>No default gate.</p>
      )}
      <form
        onSubmit={async (event) => {
          event.preventDefault();
          setError("");
          try {
            await write({
              action: "set_default_gate",
              gate: parseGate(command, timeout),
            });
          } catch (failure) {
            setError(errorText(failure));
          }
        }}
      >
        <fieldset disabled={busy}>
          <GateEditor
            command={command}
            timeout={timeout}
            onCommand={setCommand}
            onTimeout={setTimeout}
          />
          {error && <p role="alert">{error}</p>}
          <Button type="submit">Save team default gate</Button>
        </fieldset>
      </form>
    </details>
  );
}

function TaskEditor({
  task,
  tasks,
  write,
  busy,
  onClose,
}: {
  task?: ReviewTask;
  tasks: ReviewTask[];
  write: Write;
  busy: boolean;
  onClose: () => void;
}) {
  const [title, setTitle] = useState(task?.title || "");
  const [description, setDescription] = useState(task?.description || "");
  const [owner, setOwner] = useState(task?.owner || "");
  const [dependencies, setDependencies] = useState(task?.dependencies || []);
  const [status, setStatus] = useState<"ready" | "blocked" | "">(
    task ? "" : "ready",
  );
  const initial = gateFields(task?.gate);
  const [mode, setMode] = useState(task ? "custom" : "inherit");
  const [command, setCommand] = useState(initial.command);
  const [timeout, setTimeout] = useState(initial.timeout);
  const [schema, setSchema] = useState(
    task?.resultSchema === undefined ? "" : prettyJson(task.resultSchema),
  );
  const [error, setError] = useState("");
  let schemaError = "";
  try {
    parseResultSchema(schema);
  } catch (failure) {
    schemaError = errorText(failure);
  }
  return (
    <form
      className="task-review-card"
      aria-label={task ? "Update task" : "Create task"}
      onSubmit={async (event) => {
        event.preventDefault();
        setError("");
        try {
          const resultSchema = parseResultSchema(schema);
          if (task?.resultSchema !== undefined && resultSchema === undefined)
            throw new Error(
              "Use true to allow any JSON result. An existing result schema cannot be removed.",
            );
          const body: WorkBody = {
            action: task ? "update" : "create",
            title: title.trim(),
            description,
            owner: owner.trim() || null,
            dependencies,
            ...(task ? { task_id: task.id, version: task.version } : {}),
            ...(status ? { status } : {}),
            ...(mode === "custom" ? { gate: parseGate(command, timeout) } : {}),
            ...(resultSchema === undefined
              ? {}
              : { result_schema: resultSchema }),
          };
          if (await write(body)) onClose();
        } catch (failure) {
          setError(errorText(failure));
        }
      }}
    >
      <h3>{task ? "Update task" : "Create task"}</h3>
      <fieldset disabled={busy}>
        <TextInput
          label="Title"
          required
          value={title}
          onChange={(e) => setTitle(e.currentTarget.value)}
        />
        <Textarea
          label="Description"
          autosize
          minRows={3}
          value={description}
          onChange={(e) => setDescription(e.currentTarget.value)}
        />
        <TextInput
          label="Owner agent ID"
          value={owner}
          onChange={(e) => setOwner(e.currentTarget.value)}
        />
        <NativeSelect
          label="Task status"
          value={status}
          onChange={(e) => setStatus(e.currentTarget.value as typeof status)}
        >
          {task && <option value="">Keep {task.status}</option>}
          <option value="ready">ready</option>
          <option value="blocked">blocked</option>
        </NativeSelect>
        <label className="task-review-dependencies">
          Dependencies
          <select
            multiple
            aria-label="Dependencies"
            value={dependencies}
            onChange={(e) =>
              setDependencies(
                Array.from(
                  e.currentTarget.selectedOptions,
                  (option) => option.value,
                ),
              )
            }
          >
            {tasks
              .filter((candidate) => candidate.id !== task?.id)
              .map((candidate) => (
                <option key={candidate.id} value={candidate.id}>
                  {candidate.title} ({taskStatus(candidate)})
                </option>
              ))}
          </select>
        </label>
        {!task && (
          <NativeSelect
            label="Task gate"
            value={mode}
            onChange={(e) => setMode(e.currentTarget.value)}
            data={[
              { value: "inherit", label: "Use team default" },
              { value: "custom", label: "Set task gate" },
            ]}
          />
        )}
        {mode === "custom" && (
          <GateEditor
            command={command}
            timeout={timeout}
            onCommand={setCommand}
            onTimeout={setTimeout}
          />
        )}
        <Textarea
          label="Result schema (JSON)"
          description="The server validates the schema before it saves the task. Use true to allow any JSON result."
          autosize
          minRows={5}
          value={schema}
          error={schemaError || undefined}
          onChange={(e) => setSchema(e.currentTarget.value)}
        />
        {error && <p role="alert">{error}</p>}
        <div className="task-review-actions">
          <Button type="submit" disabled={!title.trim() || !!schemaError}>
            Save task
          </Button>
          <Button variant="default" onClick={onClose}>
            Cancel edit
          </Button>
        </div>
      </fieldset>
    </form>
  );
}

function TaskDetail({
  task,
  write,
  busy,
  onEdit,
}: {
  task: ReviewTask;
  write: Write;
  busy: boolean;
  onEdit: () => void;
}) {
  const { apiDownload } = useServerApi();
  const [reason, setReason] = useState("");
  const [overrideReason, setOverrideReason] = useState("");
  const [logError, setLogError] = useState("");
  const latest = task.results?.at(-1);
  const gate = latest?.gate;
  const override = requiresGateOverride(task);
  const refusal = task.submissionRefusal;
  const decide = (action: "accept" | "reject") => {
    if (
      !reason.trim() ||
      (action === "accept" && override && !overrideReason.trim())
    )
      return;
    return write({
      action,
      task_id: task.id,
      version: task.version,
      result: reason.trim(),
      ...(action === "accept" && override
        ? { override_reason: overrideReason.trim() }
        : {}),
    });
  };
  return (
    <article className="task-review-card" aria-label="Task detail">
      <div className="task-review-actions">
        <h3>{task.title}</h3>
        <Badge>{taskStatus(task)}</Badge>
        <Button
          variant="default"
          disabled={
            busy || task.status === "review" || task.status === "accepted"
          }
          onClick={onEdit}
        >
          Update task
        </Button>
      </div>
      {task.status === "review" && (
        <p>Reject the result before you edit this task.</p>
      )}
      {task.status === "accepted" && (
        <p>Create a follow-up task to change accepted work.</p>
      )}
      <p className="task-review-description">
        {task.description || "No description."}
      </p>
      <dl>
        <dt>Task ID</dt>
        <dd>{task.id}</dd>
        <dt>Owner</dt>
        <dd>{task.owner || "Unassigned"}</dd>
        <dt>Dependencies</dt>
        <dd>{task.dependencies?.join(", ") || "None"}</dd>
        <dt>Blocked by</dt>
        <dd>{task.blockedBy?.join(", ") || "None"}</dd>
        <dt>Version</dt>
        <dd>{task.version}</dd>
        <dt>Revision</dt>
        <dd>{latest?.revision || "No result."}</dd>
      </dl>
      <h4>Acceptance gate</h4>
      {task.gate ? (
        <JsonView label="Task gate configuration" value={task.gate} />
      ) : (
        <p>No gate configured.</p>
      )}
      <p>
        Gate status: <strong>{gate?.status || "not_run"}</strong>
      </p>
      {gate && (
        <>
          <dl>
            <dt>Exit code</dt>
            <dd>{gate.exitCode ?? "Unknown"}</dd>
            <dt>Gate revision</dt>
            <dd>{gate.revision || "Unknown"}</dd>
          </dl>
          {gate.error && <p role="alert">{gate.error}</p>}
          <pre className="task-review-json" aria-label="Gate output">
            {gate.output || "No output."}
          </pre>
          {gate.log && <p>Saved log: {gate.log}</p>}
          {gate.monitorId && (
            <Button
              variant="subtle"
              onClick={async () => {
                setLogError("");
                try {
                  const result = await apiDownload("/api/monitor/log", {
                    id: gate.monitorId!,
                  });
                  const url = URL.createObjectURL(result.blob);
                  const link = document.createElement("a");
                  link.href = url;
                  link.download = result.name;
                  link.click();
                  setTimeout(() => URL.revokeObjectURL(url), 10000);
                  if (result.truncated)
                    setLogError(
                      "The download contains the retained part of the log.",
                    );
                } catch (failure) {
                  setLogError(errorText(failure));
                }
              }}
            >
              Download gate log
            </Button>
          )}
          {logError && <p role="alert">{logError}</p>}
        </>
      )}
      <h4>Result schema</h4>
      {task.resultSchema === undefined ? (
        <p>No result schema.</p>
      ) : (
        <JsonView label="Result schema" value={task.resultSchema} />
      )}
      {refusal && (
        <section aria-label="Submission refusals">
          <h4>Submission refusals</h4>
          <p>
            {refusal.failedSubmits} of {refusal.limit} attempts refused.
          </p>
          {!refusal.retryAllowed && (
            <p role="alert">
              The submission limit is reached. Update the task to allow another
              attempt.
            </p>
          )}
          <ul>
            {refusal.errors.map((error, index) => (
              <li key={index}>
                <code>{error.path || "/"}</code>: {error.message}
              </li>
            ))}
          </ul>
        </section>
      )}
      <h4>Latest result</h4>
      {latest ? (
        <>
          <JsonView
            label="Latest result JSON"
            value={latest.result === undefined ? latest.text : latest.result}
          />
          <p>Checks: {latest.checks}</p>
        </>
      ) : (
        <p>No result submitted.</p>
      )}
      {task.status === "review" && (
        <fieldset disabled={busy}>
          <legend>Review decision</legend>
          <Textarea
            label="Decision reason"
            required
            value={reason}
            onChange={(e) => setReason(e.currentTarget.value)}
          />
          {override && (
            <>
              <p>
                The gate has not passed for this configuration. Acceptance
                requires an override reason.
              </p>
              <Textarea
                label="Gate override reason"
                required
                value={overrideReason}
                onChange={(e) => setOverrideReason(e.currentTarget.value)}
              />
            </>
          )}
          <div className="task-review-actions">
            <Button
              disabled={
                !reason.trim() ||
                (override && !overrideReason.trim()) ||
                gate?.status === "running"
              }
              onClick={() => void decide("accept")}
            >
              {override ? "Accept with override" : "Accept task"}
            </Button>
            <Button
              color="red"
              variant="outline"
              disabled={!reason.trim()}
              onClick={() => void decide("reject")}
            >
              Reject task
            </Button>
          </div>
          {gate?.status === "running" && (
            <p>Wait for the gate to finish before acceptance.</p>
          )}
        </fieldset>
      )}
      <details>
        <summary>Result history ({task.results?.length || 0})</summary>
        {task.results?.map((result) => (
          <section key={result.id}>
            <h4>
              {result.revision} by {result.agent}
            </h4>
            <JsonView label={`Result ${result.id}`} value={result} />
          </section>
        ))}
      </details>
      <details>
        <summary>Decision history ({task.decisions?.length || 0})</summary>
        {task.decisions?.map((decision, index) => (
          <JsonView
            key={index}
            label={`Decision ${index + 1}`}
            value={decision}
          />
        ))}
      </details>
    </article>
  );
}

export default function Tasks({
  agentId,
  revision = 0,
}: {
  agentId: string;
  revision?: number;
}) {
  const { post } = useServerApi();
  const storage = useServerStorage();
  const [reload, setReload] = useState(0);
  const resource = useWorkspaceResource("/api/work", `${revision}:${reload}`, {
    query: { agent: agentId },
  });
  const [selectedId, setSelectedId] = useState("");
  const [status, setStatus] = useState("");
  const [owner, setOwner] = useState("");
  const [editing, setEditing] = useState<"create" | "update" | null>(null);
  const [busy, setBusy] = useState(false);
  const busyRef = useRef(false);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const tasks = resource.data?.items || [];
  const selected = tasks.find((task) => task.id === selectedId);
  const visibleTasks = tasks.filter(
    (task) =>
      (!status || taskStatus(task) === status) &&
      (!owner || (task.owner || "unassigned") === owner),
  );
  const write: Write = async (body) => {
    if (busyRef.current || !resource.data?.rootId) return false;
    busyRef.current = true;
    setBusy(true);
    setError("");
    setNotice("");
    try {
      const payload = { ...body, agent: resource.data.rootId };
      const identity = await createMutationJournal("task-review", storage)(
        "/api/work",
        payload,
      );
      await post("/api/work", { ...payload, id: identity!.requestId });
      identity!.finish();
      setReload((value) => value + 1);
      setNotice("The task action completed.");
      return true;
    } catch (failure) {
      setError(errorText(failure));
      return false;
    } finally {
      busyRef.current = false;
      setBusy(false);
    }
  };
  return (
    <section className="task-review" aria-label="Tasks">
      <div className="task-review-actions">
        <Button
          disabled={busy || !resource.data?.rootId}
          onClick={() => {
            setEditing("create");
            setError("");
          }}
        >
          Create task
        </Button>
        <Button
          variant="default"
          disabled={busy}
          onClick={() => setReload((value) => value + 1)}
        >
          Refresh tasks
        </Button>
      </div>
      {(resource.error || error) && (
        <p role="alert">{resource.error || error}</p>
      )}
      {notice && <p role="status">{notice}</p>}
      {resource.loading && <p>Load tasks...</p>}
      {resource.data && (
        <DefaultGate
          key={prettyJson(resource.data.taskDefaultGate)}
          gate={resource.data.taskDefaultGate}
          write={write}
          busy={busy}
        />
      )}
      <div className="task-review-filters">
        <NativeSelect
          label="Filter by status"
          value={status}
          onChange={(e) => setStatus(e.currentTarget.value)}
          data={[
            { value: "", label: "All statuses" },
            ...[
              "ready",
              "blocked",
              "running",
              "review",
              "accepted",
              "cancelled",
            ].map((value) => ({ value, label: value })),
          ]}
        />
        <NativeSelect
          label="Filter by owner"
          value={owner}
          onChange={(e) => setOwner(e.currentTarget.value)}
          data={[
            { value: "", label: "All owners" },
            { value: "unassigned", label: "Unassigned" },
            ...Array.from(
              new Set(
                tasks.flatMap((task) => (task.owner ? [task.owner] : [])),
              ),
            ).map((value) => ({ value, label: value })),
          ]}
        />
      </div>
      <ul className="task-review-list">
        {visibleTasks.map((task) => (
          <li key={task.id}>
            <button
              disabled={busy}
              aria-pressed={selectedId === task.id}
              onClick={() => {
                setSelectedId(task.id);
                setEditing(null);
                setError("");
              }}
            >
              <strong>{task.title}</strong>
              <span>
                {taskStatus(task)} · {task.owner || "Unassigned"}
              </span>
              <span>
                Dependencies: {task.dependencies?.join(", ") || "None"}
              </span>
            </button>
          </li>
        ))}
      </ul>
      {resource.data && visibleTasks.length === 0 && (
        <p>{tasks.length ? "No tasks match these filters." : "No tasks."}</p>
      )}
      {editing && (editing === "create" || selected) && (
        <TaskEditor
          key={editing === "create" ? "create" : selected!.id}
          task={editing === "update" ? selected : undefined}
          tasks={tasks}
          write={write}
          busy={busy}
          onClose={() => setEditing(null)}
        />
      )}
      {selected && !editing && (
        <TaskDetail
          key={`${selected.id}:${selected.version}`}
          task={selected}
          write={write}
          busy={busy}
          onEdit={() => setEditing("update")}
        />
      )}
    </section>
  );
}
