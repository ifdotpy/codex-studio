import { QuestionDecision } from "../../generated/api";
import {
  useServerApi,
  useServerLocalDrafts,
  useServerServices,
  useServerStorage,
} from "../../servers/ServerServicesContext";
import MessageDate from "../conversation/transcript/MessageDate";
import ErrorDescription from "../ErrorDescription";
import { Button } from "@mantine/core";
import { MessageCircleQuestion, ShieldQuestion } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { errorText } from "../../api";
import { requestDraftsForStorage } from "./requestDrafts";
import type { components } from "../../generated/api";
import type { Json, JsonValue, Agent } from "../../types";
import "./request-questions.css";
import { nativeThreadError } from "../../nativeErrors";
import AnswerFields, {
  answerList,
  type AnswerValue,
  type AnswerValues,
} from "./AnswerFields";
import {
  browserOpenDetails,
  elicitationSchema,
  isFieldRequest,
  requestApprovalDetails,
  requestQuestions,
} from "./requestData";
import {
  approvalCreated,
  approvalParams,
  approvalThreadId,
  isAgentQuestion,
  isDeferred,
  type Approval,
} from "../../approvals";
import { approveSignInPage } from "./openSignIn";
import SignInCard from "./SignInCard";

type Props = {
  mainAgentId?: string | undefined;
  showDates?: boolean;
  onAnswerOpen?: () => void;
  onAnswerPosition?: (node: HTMLElement) => void;
  requests: Approval[];
  allRequests: Approval[];
  scope: string;
  agents: Array<
    Pick<Agent, "id" | "name" | "threadId" | "nativeThreadBlock" | "error">
  >;
  refresh: () => Promise<void>;
  notify: (s: string) => void;
};

type RequestDto = Approval;
type QuestionCardAnswer = components["schemas"]["QuestionCardAnswer"];
type JsonObject = Record<string, JsonValue>;

function isJsonObject(value: unknown): value is JsonObject {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function stringValue(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
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
  // The requested names, when the provider sent no details.
  if (Array.isArray(value) && value.every((entry) => typeof entry === "string"))
    return value.map((entry): [string, string] => [entry, "Requested"]);
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

function useAnswerDrafts() {
  return requestDraftsForStorage(useServerStorage());
}
const answerKey = (scope: string, request: RequestDto) =>
  JSON.stringify([
    scope,
    request.agent,
    request.id,
    request.request.kind,
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

export function AnswerForm({
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
  post: (body: Json) => Promise<boolean>;
  close: () => void;
  notify: Props["notify"];
}) {
  const { saved } = useServerApi();
  const { writeLocalDraft } = useServerLocalDrafts();
  const answerDrafts = useAnswerDrafts();

  const [values, setValues] = useState<AnswerValues>(
    () =>
      answerDrafts.get(draftKey) ||
      saved(`studio-answer-draft:${draftKey}`, {}),
  );
  const questions = requestQuestions(request);
  const kind = request.request.kind;
  const schema = elicitationSchema(request);
  const properties = schema?.properties ?? {};
  const elicitation = kind === "elicitation";
  const required = new Set(elicitation ? (schema?.required ?? []) : []);
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
      let answer: QuestionCardAnswer;
      if (kind === "question" || kind === "agent_question") {
        answer = {
          method:
            kind === "question"
              ? "item/tool/requestUserInput"
              : "agent/asyncQuestion",
          data: {
            answers: Object.fromEntries(
              questions.map((q) => [
                q.id,
                { answers: answerList(q, values[q.id]) },
              ]),
            ),
          },
        };
      } else if (kind === "elicitation") {
        const content: components["schemas"]["QuestionElicitation"]["content"] =
          {};
        for (const [key, value] of Object.entries(properties)) {
          const schemaProperty = value;
          const type = stringValue(schemaProperty.type) || "string";
          const enumValues = schemaProperty.items?.enum ?? [];
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
                    ? parseJson(v[0] ?? "")
                    : v[0];
        }
        answer = {
          method: "mcpServer/elicitation/request",
          data: { decision: QuestionDecision.accept, content },
        };
      } else {
        throw new Error("Unsupported question type.");
      }
      void post(answer.data);
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
          disabled={sending || !ready}
          loading={sending}
        >
          Send
        </Button>
      </div>
    </form>
  );
}

export function RequestCard({
  request: r,
  messageId,
  onAnswerOpen,
  onAnswerPosition,
  showDates = true,
  agents,
  refresh,
  notify,
  scope,
}: Omit<Props, "requests" | "allRequests"> & {
  request: RequestDto;
  messageId?: string;
}) {
  const { post: apiPost } = useServerApi();
  const services = useServerServices();
  const { writeLocalDraft } = useServerLocalDrafts();
  const answerDrafts = useAnswerDrafts();

  const draftKey = answerKey(scope, r);
  const owner = agents.find((a) => a.id === r.agent);
  const params = approvalParams(r);
  const approvalDetails = requestApprovalDetails(r);
  const browserOpen = browserOpenDetails(r);
  const kind = r.request.kind;
  const requestThread = approvalThreadId(r);
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
  /** Send one answer; true when the server accepted it. */
  const post = async (body: Json): Promise<boolean> => {
    if (blocked) {
      notify("This chat stopped as a precaution. Open another chat.");
      return false;
    }
    if (pending.current) return false;
    pending.current = true;
    setSending(true);
    setRequestError("");
    try {
      const approve = () => apiPost("/api/answer", { ...body, id: r.id });
      if (browserOpen && body.decision === "accept")
        await approveSignInPage({
          url: browserOpen.url,
          requestId: r.id,
          owner: services?.owner.id,
          approve,
          onOpenError: (error) =>
            notify(
              `Studio approved the link, but could not confirm that its page opened: ${errorText(error)}`,
            ),
        });
      else await approve();
      answerDrafts.delete(draftKey);
      const storageError = writeLocalDraft(
        `studio-answer-draft:${draftKey}`,
        null,
      );
      if (storageError) notify(storageError);
      if (!mounted.current) return true;
      setOpen(false);
      await refresh();
      return true;
    } catch (e) {
      if (mounted.current) {
        setRequestError(errorText(e));
        notify(errorText(e));
      }
      return false;
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
          deferred: !isDeferred(r),
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
  const asynchronous = isAgentQuestion(r);
  const stdinApproval = kind === "exec" && params.kind === "writeStdin";
  const question = isFieldRequest(r);
  // Every other kind (and a link elicitation) is a yes or no approval.
  const approval = !question;
  const { reason, message, command, cwd, files, permissions, url, tool } =
    approvalDetails;
  const questions = question ? requestQuestions(r) : [];
  // The controls need a pending request; other states only say where it stands.
  const waiting = r.state.kind === "pending";
  const stateNote = {
    pending: "",
    answering: "Sending your answer",
    answered: "Answered",
    unknown: "The agent may not have got your answer",
    closed: "Closed",
  }[r.state.kind];
  const Icon = approval ? ShieldQuestion : MessageCircleQuestion;
  if (browserOpen)
    return (
      <div
        className="request request-card request-sign-in"
        ref={card}
        data-request={r.id}
        data-message={messageId}
      >
        {showDates && <MessageDate at={approvalCreated(r)} />}
        {blocked ? (
          <p>
            This chat stopped as a precaution. This request cannot resume it.
          </p>
        ) : (
          <SignInCard
            details={browserOpen}
            agentName={owner?.name || "Codex"}
            chatNames={
              new Map(agents.map((agent) => [agent.id, agent.name || ""]))
            }
            serverName={services?.owner.alias || services?.owner.label}
            sending={sending}
            error={requestError}
            post={post}
            notify={notify}
          />
        )}
      </div>
    );
  return (
    <div
      className={`request request-card${asynchronous ? " request-async" : ""}`}
      ref={card}
      data-request={r.id}
      data-message={messageId}
      onFocusCapture={(event) => {
        const target = event.target;
        if (target instanceof HTMLElement) {
          const option = target.closest<HTMLElement>(".request-answer-option");
          option?.scrollIntoView({ block: "nearest" });
        }
      }}
    >
      <div className="request-summary">
        <Icon size={18} className="request-icon" />
        <div className="request-copy">
          <div className="request-heading">
            <strong>
              {agents.find((a) => a.id === r.agent)?.name || "Codex"}
            </strong>
            <span
              className={`request-status${question && waiting ? " request-status-hidden" : ""}`}
            >
              {!waiting
                ? stateNote
                : isDeferred(r)
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
          {showDates && <MessageDate at={approvalCreated(r)} />}
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
            <pre className="request-command">{command}</pre>
          )}
          {files !== undefined && files.length > 0 && (
            <pre className="request-command">{files.join("\n")}</pre>
          )}
          {tool !== undefined && <p className="request-cwd">Tool {tool}</p>}
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
          {question && waiting && (
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
          ) : !waiting ? null : question ? (
            <>
              {!open && (
                <Button
                  variant="subtle"
                  disabled={sending}
                  aria-label={isDeferred(r) ? "Restore" : "Defer"}
                  onClick={() => void defer()}
                >
                  {isDeferred(r) ? "Restore" : "Answer later"}
                </Button>
              )}
              <Button
                ref={trigger}
                data-answer={r.id}
                variant={open ? "subtle" : "filled"}
                {...(open ? { color: "gray" } : {})}
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
                onClick={() =>
                  void post({
                    decision: QuestionDecision.decline,
                  } satisfies components["schemas"]["QuestionApproval"])
                }
              >
                {stdinApproval ? "Decline input" : "Decline"}
              </Button>
              <Button
                variant="filled"
                disabled={sending}
                onClick={() =>
                  void post({
                    decision: QuestionDecision.accept,
                  } satisfies components["schemas"]["QuestionApproval"])
                }
              >
                {stdinApproval ? "Allow input" : "Approve"}
              </Button>
            </>
          ) : null}
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

/** Open the answer form of a pending request card, as its own Answer button does. */
export function openRequestAnswer(requestId: string): boolean {
  const trigger = [
    ...document.querySelectorAll<HTMLButtonElement>("#requests [data-answer]"),
  ].find((button) => button.dataset.answer === requestId);
  if (!trigger) return false;
  const later = trigger.closest("details");
  if (later) later.open = true;
  if (trigger.getAttribute("aria-expanded") === "true") {
    trigger.scrollIntoView({ block: "nearest" });
    trigger.focus();
  } else trigger.click();
  return true;
}

export default function Requests({
  requests,
  allRequests,
  mainAgentId,
  ...props
}: Props) {
  const answerDrafts = useAnswerDrafts();
  useEffect(() => {
    const active = new Set(
      allRequests.map((request) => answerKey(props.scope, request)),
    );
    for (const key of answerDrafts.keys())
      if (JSON.parse(key)[0] === props.scope && !active.has(key))
        answerDrafts.delete(key);
  }, [allRequests, props.scope, answerDrafts]);
  const group = (items: RequestDto[]) => {
    const deferred = items.filter(isDeferred);
    return (
      <>
        {items
          .filter((r) => !isDeferred(r))
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
    <div id="requests">
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
