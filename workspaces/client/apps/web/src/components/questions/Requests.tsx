import MessageDate from "../conversation/transcript/MessageDate";
import ErrorDescription from "../ErrorDescription";
import { Button } from "@mantine/core";
import { MessageCircleQuestion, ShieldQuestion } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { post as apiPost, errorText, saved } from "../../api";
import { writeLocalDraft } from "../../sync/localDraft";
import type { components } from "../../generated/api";
import type { Json, JsonValue, Agent } from "../../types";
import "./request-questions.css";
import { nativeThreadError } from "../../nativeErrors";
import AnswerFields, {
  answerList,
  type AnswerValue,
  type AnswerValues,
} from "./AnswerFields";
import { requestApprovalDetails, requestQuestions } from "./requestData";

type Props = {
  mainAgentId?: string;
  showDates?: boolean;
  onAnswerOpen?: () => void;
  onAnswerPosition?: (node: HTMLElement) => void;
  requests: components["schemas"]["RequestEntityDto"][];
  allRequests: components["schemas"]["RequestEntityDto"][];
  scope: string;
  agents: Agent[];
  refresh: () => Promise<void>;
  notify: (s: string) => void;
};

type RequestDto = components["schemas"]["RequestEntityDto"];
type JsonObject = Record<string, JsonValue>;

function isJsonObject(value: unknown): value is JsonObject {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function objectValue(value: unknown): JsonObject {
  return isJsonObject(value) ? value : {};
}

function stringValue(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}

function stringValues(value: unknown): string[] {
  return Array.isArray(value)
    ? value.filter((entry): entry is string => typeof entry === "string")
    : [];
}

function isJsonValue(value: unknown): value is JsonValue {
  if (
    value === null ||
    typeof value === "string" ||
    typeof value === "number" ||
    typeof value === "boolean"
  )
    return true;
  if (Array.isArray(value)) return value.every(isJsonValue);
  return typeof value === "object" && Object.values(value).every(isJsonValue);
}

function parseJson(text: string): JsonValue {
  const value: unknown = JSON.parse(text);
  if (!isJsonValue(value)) throw new Error("Enter a valid JSON value.");
  return value;
}

function permissionRows(
  value: unknown,
  name = "Permission",
): [string, string][] {
  if (isJsonObject(value)) {
    return Object.entries(value).flatMap(([key, entry]) =>
      permissionRows(entry, name === "Permission" ? key : `${name} / ${key}`),
    );
  }
  if (Array.isArray(value))
    return value.flatMap((entry, index) =>
      permissionRows(entry, `${name} ${index + 1}`),
    );
  return [
    [
      name.replace(/([a-z])([A-Z])/g, "$1 $2").replaceAll("_", " "),
      value === true
        ? "Allowed"
        : value === false
          ? "Not allowed"
          : value == null
            ? "None"
            : String(value),
    ],
  ];
}

// Secret answers remain in memory. Other answers survive a reload.
const answerDrafts = new Map<string, AnswerValues>();
// Show a command as the shell line the user would type, not as JSON.
function commandText(command: unknown): string {
  if (typeof command === "string") return command;
  if (
    Array.isArray(command) &&
    command.every((part) => typeof part === "string")
  )
    return command
      .map((part) =>
        /^[\w@%+=:,./-]+$/.test(part)
          ? part
          : `'${part.replace(/'/g, `'\\''`)}'`,
      )
      .join(" ");
  return JSON.stringify(command, null, 2);
}

const answerKey = (scope: string, request: RequestDto) =>
  JSON.stringify([
    scope,
    request.agent,
    request.id,
    request.method,
    requestQuestions(request).map(
      ({ id, question, isSecret, isOther, multiSelect, options }) => ({
        id,
        question,
        isSecret,
        isOther,
        multiSelect,
        options,
      }),
    ),
  ]);

function AnswerForm({
  request,
  sending,
  post,
  close,
  notify,
  draftKey,
}: {
  draftKey: string;
  request: RequestDto;
  sending: boolean;
  post: (body: Json) => Promise<void>;
  close: () => void;
  notify: Props["notify"];
}) {
  const [values, setValues] = useState<AnswerValues>(
    () =>
      answerDrafts.get(draftKey) ||
      saved(`studio-answer-draft:${draftKey}`, {}),
  );
  const questions = requestQuestions(request);
  const method = request.method || "";
  const params = objectValue(request.params);
  const schema = objectValue(params.requestedSchema);
  const properties = objectValue(schema.properties);
  const elicitation = method === "mcpServer/elicitation/request";
  const required = new Set(stringValues(elicitation ? schema.required : []));
  const missingRequired = [...required].some(
    (id) => !questions.some((question) => question.id === id),
  );
  const setValue = (id: string, value: AnswerValue) => {
    const next = { ...values, [id]: value };
    answerDrafts.set(draftKey, next);
    setValues(next);
    const publicValues = Object.fromEntries(
      questions.filter((q) => !q.isSecret).map((q) => [q.id, next[q.id] || ""]),
    );
    const error = writeLocalDraft(
      `studio-answer-draft:${draftKey}`,
      publicValues,
    );
    if (error) notify(error);
  };
  const ready =
    !missingRequired &&
    (elicitation || questions.length > 0) &&
    questions
      .filter((q) => !elicitation || required.has(q.id))
      .every((q) =>
        answerList(q, values[q.id]).some((answer) => answer.trim()),
      );
  const submit = (e: React.FormEvent) => {
    e.preventDefault();
    if (sending || !ready) return;
    try {
      let body: Json;
      if (
        ["item/tool/requestUserInput", "agent/asyncQuestion"].includes(method)
      ) {
        body = {
          answers: Object.fromEntries(
            questions.map((q) => [
              q.id,
              { answers: answerList(q, values[q.id]) },
            ]),
          ),
        };
      } else {
        const content: Json = {};
        for (const [key, value] of Object.entries(properties)) {
          const schemaProperty = objectValue(value);
          const type = stringValue(schemaProperty.type) || "string";
          const propertyItems = objectValue(schemaProperty.items);
          const enumValues = stringValues(propertyItems.enum);
          const question = questions.find((entry) => entry.id === key);
          if (!question)
            throw new Error("The requested answer fields changed.");
          const v = answerList(question, values[key]);
          if (!required.has(key) && !v.some((answer) => answer.trim()))
            continue;
          content[key] =
            type === "boolean"
              ? v[0] === "true"
              : ["integer", "number"].includes(type)
                ? Number(v[0])
                : type === "array" && enumValues.length > 0
                  ? v
                  : ["object", "array"].includes(type)
                    ? parseJson(v[0])
                    : v[0];
        }
        body = { decision: "accept", content };
      }
      void post(body);
    } catch (e) {
      notify(errorText(e));
    }
  };
  return (
    <form
      className="request-answer-form"
      aria-label="Reply to the agent"
      onSubmit={submit}
      onKeyDown={(e) => {
        if (e.key === "Escape") {
          e.stopPropagation();
          if (!sending) close();
        }
      }}
    >
      <AnswerFields
        questions={questions}
        values={values}
        disabled={sending}
        onChange={setValue}
      />
      <div className="request-answer-actions">
        <Button
          type="button"
          aria-label="Close reply"
          variant="subtle"
          disabled={sending}
          onClick={() => {
            close();
          }}
        >
          Answer later
        </Button>
        <Button
          type="submit"
          aria-label="Send answer"
          variant="filled"
          color="indigo"
          disabled={sending || !ready}
          loading={sending}
        >
          Send
        </Button>
      </div>
    </form>
  );
}

function RequestCard({
  request: r,
  onAnswerOpen,
  onAnswerPosition,
  showDates = true,
  agents,
  refresh,
  notify,
  scope,
}: Omit<Props, "requests" | "allRequests"> & { request: RequestDto }) {
  const draftKey = answerKey(scope, r);
  const owner = agents.find((a) => a.id === r.agent);
  const params = objectValue(r.params);
  const approvalDetails = requestApprovalDetails(r);
  const method = r.method || "";
  const requestThread = stringValue(params.threadId);
  const blocked =
    !!nativeThreadError(owner) && requestThread === owner?.threadId;
  const [requestError, setRequestError] = useState("");
  const [open, setOpen] = useState(false),
    [sending, setSending] = useState(false);
  const card = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open || !card.current) return;
    const node = card.current;
    const form = node.querySelector<HTMLElement>(".request-answer-form");
    const transcript = node.closest<HTMLElement>("#messages");
    const requests = node.closest<HTMLElement>("#requests");
    if (!form || !transcript) return;
    const position = () => {
      if (requests) {
        const cardBounds = node.getBoundingClientRect();
        const requestsBounds = requests.getBoundingClientRect();
        if (
          cardBounds.top < requestsBounds.top ||
          cardBounds.top >= requestsBounds.bottom
        )
          requests.scrollTop += cardBounds.top - requestsBounds.top - 12;
      }
      if (onAnswerPosition) onAnswerPosition(node);
      else
        transcript.scrollTop +=
          node.getBoundingClientRect().top -
          transcript.getBoundingClientRect().top -
          12;
    };
    const observer = new ResizeObserver(() => position());
    observer.observe(transcript);
    if (requests) observer.observe(requests);
    position();
    const frame = requestAnimationFrame(() => position());
    return () => {
      cancelAnimationFrame(frame);
      observer.disconnect();
    };
  }, [open]);
  const pending = useRef(false),
    mounted = useRef(true),
    trigger = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
  const close = () => {
    setOpen(false);
    trigger.current?.focus();
  };
  const post = async (body: Json) => {
    if (blocked) {
      notify("This chat stopped as a precaution. Open another chat.");
      return;
    }
    if (pending.current) return;
    pending.current = true;
    setSending(true);
    setRequestError("");
    try {
      await apiPost("/api/answer", { ...body, id: r.id });
      answerDrafts.delete(draftKey);
      const storageError = writeLocalDraft(
        `studio-answer-draft:${draftKey}`,
        null,
      );
      if (storageError) notify(storageError);
      if (!mounted.current) return;
      setOpen(false);
      await refresh();
    } catch (e) {
      if (mounted.current) {
        setRequestError(errorText(e));
        notify(errorText(e));
      }
    } finally {
      pending.current = false;
      if (mounted.current) setSending(false);
    }
  };
  const defer = async (remove = false) => {
    if (pending.current) return;
    pending.current = true;
    setSending(true);
    setRequestError("");
    try {
      if (remove) await apiPost("/api/questions/delete", { id: r.id });
      else
        await apiPost("/api/questions/defer", {
          id: r.id,
          deferred: !r.deferred,
        });
      if (mounted.current) setOpen(false);
      await refresh();
    } catch (error) {
      if (mounted.current) {
        setRequestError(errorText(error));
        notify(errorText(error));
      }
    } finally {
      pending.current = false;
      if (mounted.current) setSending(false);
    }
  };
  const asynchronous = method === "agent/asyncQuestion";
  const stdinApproval =
    method === "item/commandExecution/requestApproval" &&
    params.kind === "writeStdin";
  const question =
    ["item/tool/requestUserInput", "agent/asyncQuestion"].includes(method) ||
    (method === "mcpServer/elicitation/request" && params.mode === "form");
  const approval =
    [
      "monitor/approve",
      "item/commandExecution/requestApproval",
      "item/fileChange/requestApproval",
      "item/permissions/requestApproval",
    ].includes(method) ||
    (method === "mcpServer/elicitation/request" && params.mode === "url");
  const reason = stringValue(params.reason);
  const message = stringValue(params.message);
  const command = approvalDetails.command;
  const cwd = stringValue(params.cwd);
  const permissions = approvalDetails.permissions;
  const url = stringValue(params.url);
  const questions = question ? requestQuestions(r) : [];
  const Icon = approval ? ShieldQuestion : MessageCircleQuestion;
  return (
    <div
      className={`request request-card${asynchronous ? " request-async" : ""}`}
      ref={card}
      data-request={r.id}
    >
      <div className="request-summary">
        <Icon size={18} className="request-icon" />
        <div className="request-copy">
          <div className="request-heading">
            <strong>
              {agents.find((a) => a.id === r.agent)?.name || "Codex"}
            </strong>
            <span
              className={`request-status${question ? " request-status-hidden" : ""}`}
            >
              {r.deferred
                ? asynchronous
                  ? "Deferred · Agent can continue"
                  : "Deferred · Agent still waits"
                : asynchronous
                  ? "Agent can continue"
                  : question
                    ? "Answer required"
                    : approval
                      ? stdinApproval
                        ? "Terminal input approval required"
                        : "Approval required"
                      : "Request"}
            </span>
          </div>
          {showDates && <MessageDate at={r.createdAt ?? r.created ?? r.at} />}
          {(!open || blocked || !question) &&
            (questions[0]?.question || reason || message || !approval) && (
              <p className="request-prompt">
                {questions[0]?.question || (
                  <ErrorDescription
                    value={reason || message || "The agent needs a reply."}
                    role="status"
                  />
                )}
              </p>
            )}
          {questions[0]?.question &&
            (reason || message) &&
            (reason || message) !== questions[0].question && (
              <p>
                <ErrorDescription value={reason || message} role="status" />
              </p>
            )}
          {!open && questions.length > 1 && <p>{questions.length} questions</p>}
          {!stdinApproval && command !== undefined && (
            <pre className="request-command">{commandText(command)}</pre>
          )}
          {cwd && (
            <p className="request-cwd" title={cwd}>
              in {cwd}
            </p>
          )}
          {permissions !== undefined && (
            <div className="request-permissions">
              <dl>
                {permissionRows(permissions).map(([name, value]) => (
                  <div key={name}>
                    <dt>{name}</dt>
                    <dd title={value}>{value}</dd>
                  </div>
                ))}
              </dl>
              <details>
                <summary>Permission details</summary>
                <pre>{JSON.stringify(permissions, null, 2)}</pre>
              </details>
            </div>
          )}
          {url && /^https?:\/\//.test(url) && (
            <a href={url} target="_blank" rel="noreferrer">
              Open request
            </a>
          )}
        </div>
        <div className="request-actions">
          {question && (
            <Button
              variant="subtle"
              className="request-delete"
              disabled={sending}
              onClick={() => void defer(true)}
            >
              Delete question
            </Button>
          )}
          {blocked ? (
            <p>
              This chat stopped as a precaution. This request cannot resume it.
            </p>
          ) : question ? (
            <>
              {!open && (
                <Button
                  variant="subtle"
                  disabled={sending}
                  aria-label={r.deferred ? "Restore" : "Defer"}
                  onClick={() => void defer()}
                >
                  {r.deferred ? "Restore" : "Answer later"}
                </Button>
              )}
              <Button
                ref={trigger}
                data-answer={r.id}
                variant={open ? "subtle" : "filled"}
                color={open ? "gray" : "indigo"}
                disabled={sending}
                aria-expanded={open}
                aria-controls={`answer-${r.id}`}
                onClick={() => {
                  if (!open) onAnswerOpen?.();
                  setOpen(!open);
                }}
              >
                {open ? "Hide" : "Answer"}
              </Button>
            </>
          ) : approval ? (
            <>
              <Button
                disabled={sending}
                onClick={() => void post({ decision: "decline" })}
              >
                {stdinApproval ? "Decline input" : "Decline"}
              </Button>
              <Button
                variant="filled"
                color="indigo"
                disabled={sending}
                onClick={() => void post({ decision: "accept" })}
              >
                {stdinApproval ? "Allow input" : "Approve"}
              </Button>
            </>
          ) : (
            <p>Unsupported client request: {method}</p>
          )}
        </div>
      </div>
      {requestError && (
        <div className="studio-recovery-banner" role="alert">
          <p>Could not update this request.</p>
          <details>
            <summary>Details</summary>
            <ErrorDescription value={requestError} />
          </details>
        </div>
      )}
      {!blocked && question && open && (
        <div id={`answer-${r.id}`} className="request-inline-answer">
          <AnswerForm
            request={r}
            draftKey={draftKey}
            sending={sending}
            post={post}
            close={close}
            notify={notify}
          />
        </div>
      )}
    </div>
  );
}

export default function Requests({
  requests,
  allRequests,
  mainAgentId,
  ...props
}: Props) {
  useEffect(() => {
    const active = new Set(
      allRequests.map((request) => answerKey(props.scope, request)),
    );
    for (const key of answerDrafts.keys())
      if (JSON.parse(key)[0] === props.scope && !active.has(key))
        answerDrafts.delete(key);
  }, [allRequests, props.scope]);
  const group = (items: RequestDto[]) => {
    const deferred = items.filter((r) => r.deferred);
    return (
      <>
        {items
          .filter((r) => !r.deferred)
          .map((r) => (
            <RequestCard
              key={answerKey(props.scope, r)}
              request={r}
              {...props}
            />
          ))}
        {deferred.length > 0 && (
          <details className="request-history request-deferred">
            <summary>
              Later <span>{deferred.length}</span>
            </summary>
            {deferred.map((r) => (
              <RequestCard
                key={answerKey(props.scope, r)}
                request={r}
                {...props}
              />
            ))}
          </details>
        )}
      </>
    );
  };
  const mainRequests = mainAgentId
    ? requests.filter((r) => r.agent === mainAgentId)
    : [];
  return (
    <div
      id="requests"
      onFocusCapture={(event) => {
        const target = event.target;
        if (target instanceof HTMLElement) {
          const option = target.closest<HTMLElement>(".request-answer-option");
          option?.scrollIntoView({ block: "nearest" });
        }
      }}
    >
      {mainRequests.length > 0 && (
        <section aria-label="Main-agent requests">
          <h3 className="main-agent-requests-heading">Main-agent requests</h3>
          {group(mainRequests)}
        </section>
      )}
      {group(
        mainAgentId
          ? requests.filter((r) => r.agent !== mainAgentId)
          : requests,
      )}
    </div>
  );
}
