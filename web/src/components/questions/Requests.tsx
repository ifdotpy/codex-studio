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
  type AnswerQuestion,
  type AnswerValue,
  type AnswerValues,
} from "./AnswerFields";

type Props = {
  showDates?: boolean;
  requests: components["schemas"]["RequestEntityDto"][];
  allRequests: components["schemas"]["RequestEntityDto"][];
  scope: string;
  agents: Agent[];
  refresh: () => Promise<void>;
  notify: (s: string) => void;
};

type JsonObject = Record<string, JsonValue>;
type RequestDto = components["schemas"]["RequestEntityDto"];

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

function jsonOptions(value: unknown): AnswerQuestion["options"] {
  if (!Array.isArray(value)) return undefined;
  return value.flatMap((entry) => {
    if (typeof entry === "string") return [{ label: entry }];
    if (!isJsonObject(entry)) return [];
    const label = stringValue(entry.label);
    if (!label) return [];
    const description = stringValue(entry.description);
    return [{ label, ...(description ? { description } : {}) }];
  });
}

function requestQuestions(request: RequestDto): AnswerQuestion[] {
  const params = objectValue(request.params);
  if (Array.isArray(params.questions)) {
    return params.questions.flatMap((entry, index) => {
      if (!isJsonObject(entry)) return [];
      const id = stringValue(entry.id) || `question-${index + 1}`;
      return [
        {
          id,
          question:
            stringValue(entry.question) || stringValue(entry.title) || id,
          options: jsonOptions(entry.options),
          multiSelect: entry.multiSelect === true,
          isSecret: entry.isSecret === true,
        },
      ];
    });
  }

  const schema = objectValue(params.requestedSchema);
  const properties = objectValue(schema.properties);
  return Object.entries(properties).flatMap(([id, value]) => {
    if (!isJsonObject(value)) return [];
    const items = objectValue(value.items);
    const optionValues = stringValues(
      value.type === "array" ? items.enum : value.enum,
    );
    return [
      {
        id,
        question: stringValue(value.title) || id,
        options: optionValues.map((label) => ({ label })),
        multiSelect: value.type === "array" && optionValues.length > 0,
        isSecret:
          value.isSecret === true ||
          value.writeOnly === true ||
          value.format === "password",
      },
    ];
  });
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
      ({ id, question, isSecret, multiSelect, options }) => ({
        id,
        question,
        isSecret,
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
          variant="subtle"
          disabled={sending}
          onClick={() => {
            close();
          }}
        >
          Close reply
        </Button>
        <Button
          type="submit"
          variant="filled"
          color="indigo"
          disabled={sending || !ready}
          loading={sending}
        >
          Send answer
        </Button>
      </div>
    </form>
  );
}

function RequestCard({
  request: r,
  showDates = true,
  agents,
  refresh,
  notify,
  scope,
}: Omit<Props, "requests" | "allRequests"> & { request: RequestDto }) {
  const draftKey = answerKey(scope, r);
  const owner = agents.find((a) => a.id === r.agent);
  const params = objectValue(r.params);
  const preview = objectValue(params.preview);
  const method = r.method || "";
  const requestThread = stringValue(params.threadId);
  const blocked =
    !!nativeThreadError(owner) && requestThread === owner?.threadId;
  const [open, setOpen] = useState(false),
    [sending, setSending] = useState(false);
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
      if (mounted.current) notify(errorText(e));
    } finally {
      pending.current = false;
      if (mounted.current) setSending(false);
    }
  };
  const defer = async (remove = false) => {
    if (pending.current) return;
    pending.current = true;
    setSending(true);
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
      if (mounted.current) notify(errorText(error));
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
  const command = params.command ?? preview.command;
  const cwd = stringValue(params.cwd);
  const permissions = params.permissions ?? preview.changes;
  const url = stringValue(params.url);
  const questions = question ? requestQuestions(r) : [];
  const Icon = approval ? ShieldQuestion : MessageCircleQuestion;
  return (
    <div
      className={`request request-card${asynchronous ? " request-async" : ""}`}
      data-request={r.id}
    >
      <div className="request-summary">
        <Icon size={18} className="request-icon" />
        <div className="request-copy">
          <div className="request-heading">
            <strong>
              {agents.find((a) => a.id === r.agent)?.name || "Codex"}
            </strong>
            <span className="request-status">
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
            <pre>{JSON.stringify(permissions, null, 2)}</pre>
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
              <Button
                variant="subtle"
                disabled={sending}
                onClick={() => void defer()}
              >
                {r.deferred ? "Restore" : "Defer"}
              </Button>
              <Button
                ref={trigger}
                data-answer={r.id}
                variant={open ? "subtle" : "filled"}
                color={open ? "gray" : "indigo"}
                disabled={sending}
                aria-expanded={open}
                aria-controls={`answer-${r.id}`}
                onClick={() => setOpen(!open)}
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

export default function Requests({ requests, allRequests, ...props }: Props) {
  useEffect(() => {
    const active = new Set(
      allRequests.map((request) => answerKey(props.scope, request)),
    );
    for (const key of answerDrafts.keys())
      if (JSON.parse(key)[0] === props.scope && !active.has(key))
        answerDrafts.delete(key);
  }, [allRequests, props.scope]);
  const deferred = requests.filter((r) => r.deferred);
  return (
    <div id="requests">
      {requests
        .filter((r) => !r.deferred)
        .map((r) => (
          <RequestCard key={answerKey(props.scope, r)} request={r} {...props} />
        ))}
      {deferred.length > 0 && (
        <details className="request-history request-deferred">
          <summary>
            Deferred questions <span>{deferred.length}</span>
          </summary>
          <p className="request-deferred-note">Deferred questions stay open.</p>
          {deferred.map((r) => (
            <RequestCard
              key={answerKey(props.scope, r)}
              request={r}
              {...props}
            />
          ))}
        </details>
      )}
    </div>
  );
}
