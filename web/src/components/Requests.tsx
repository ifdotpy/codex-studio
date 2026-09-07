import {
  Button,
  Modal,
  NativeSelect,
  TextInput,
  UnstyledButton,
} from "@mantine/core";
import { MessageCircleQuestion, ShieldQuestion } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { api, errorText } from "../api";
import type { Json, Agent } from "../types";
import "./request-questions.css";

type Props = {
  requests: Json[];
  agents: Agent[];
  refresh: () => Promise<void>;
  notify: (s: string) => void;
};

function requestQuestions(request: Json): Json[] {
  return (
    request.params?.questions ||
    Object.entries(request.params?.requestedSchema?.properties || {}).map(
      ([id, p]) => ({
        id,
        question: (p as Json).title || id,
        options: (p as Json).enum?.map((label: string) => ({ label })),
      }),
    )
  );
}

function AnswerForm({
  request,
  sending,
  post,
  close,
  notify,
}: {
  request: Json;
  sending: boolean;
  post: (body: Json) => Promise<void>;
  close: () => void;
  notify: Props["notify"];
}) {
  const [values, setValues] = useState<Record<string, string>>({});
  const questions = requestQuestions(request);
  const asynchronous = request.method === "agent/asyncQuestion";
  const setValue = (id: string, value: string) =>
    setValues((current) => ({ ...current, [id]: value }));
  const ready =
    !asynchronous ||
    (questions.length > 0 && questions.every((q) => values[q.id]?.trim()));
  const submit = (e: React.FormEvent) => {
    e.preventDefault();
    if (sending || !ready) return;
    try {
      let body: Json;
      if (
        ["item/tool/requestUserInput", "agent/asyncQuestion"].includes(
          request.method,
        )
      ) {
        body = {
          answers: Object.fromEntries(
            questions.map((q) => [q.id, { answers: [values[q.id] || ""] }]),
          ),
        };
      } else {
        const content: Json = {};
        for (const [key, p] of Object.entries(
          request.params.requestedSchema.properties,
        )) {
          const type = (p as Json).type,
            v = values[key] || "";
          content[key] =
            type === "boolean"
              ? v === "true"
              : ["integer", "number"].includes(type)
                ? Number(v)
                : ["object", "array"].includes(type)
                  ? JSON.parse(v)
                  : v;
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
        if (asynchronous && e.key === "Escape") {
          e.stopPropagation();
          if (!sending) close();
        }
      }}
    >
      <fieldset disabled={sending} className="request-answer-fields">
        {questions.map((q, index) => (
          <div className="answer-field" key={q.id}>
            <p className="request-question-label">
              {questions.length > 1 && <span>{index + 1}. </span>}
              {q.question}
            </p>
            {q.options?.length > 0 &&
              (asynchronous ? (
                <div
                  className="request-answer-options"
                  role="group"
                  aria-label={`${q.question} options`}
                >
                  {q.options.map((option: Json) => (
                    <UnstyledButton
                      key={option.label}
                      type="button"
                      className="request-answer-option"
                      aria-pressed={values[q.id] === option.label}
                      onClick={() => setValue(q.id, option.label)}
                    >
                      <span>{option.label}</span>
                      {option.description && (
                        <small>{option.description}</small>
                      )}
                    </UnstyledButton>
                  ))}
                </div>
              ) : (
                <NativeSelect
                  aria-label={`${q.question} options`}
                  value={values[q.id] || ""}
                  onChange={(e) => setValue(q.id, e.target.value)}
                >
                  <option value="">Choose an answer</option>
                  {q.options.map((option: Json) => (
                    <option key={option.label} value={option.label}>
                      {option.label}
                      {option.description ? " · " + option.description : ""}
                    </option>
                  ))}
                </NativeSelect>
              ))}
            <TextInput
              aria-label={q.question}
              type={q.isSecret ? "password" : "text"}
              value={values[q.id] || ""}
              onChange={(e) => setValue(q.id, e.target.value)}
              placeholder={
                asynchronous && q.options?.length
                  ? "Or write your own answer"
                  : "Your answer"
              }
            />
          </div>
        ))}
      </fieldset>
      <div className="request-answer-actions">
        <Button
          type="button"
          variant="subtle"
          disabled={sending}
          onClick={close}
        >
          Cancel
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
  agents,
  refresh,
  notify,
}: Omit<Props, "requests"> & { request: Json }) {
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
    if (pending.current) return;
    pending.current = true;
    setSending(true);
    try {
      await api("/api/answer", { ...body, id: r.id });
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
  const p = r.params || {},
    asynchronous = r.method === "agent/asyncQuestion",
    question =
      ["item/tool/requestUserInput", "agent/asyncQuestion"].includes(
        r.method,
      ) ||
      (r.method === "mcpServer/elicitation/request" && p.mode === "form"),
    approval =
      [
        "monitor/approve",
        "item/commandExecution/requestApproval",
        "item/fileChange/requestApproval",
        "item/permissions/requestApproval",
        "applyPatchApproval",
        "execCommandApproval",
      ].includes(r.method) ||
      (r.method === "mcpServer/elicitation/request" && p.mode === "url");
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
              {asynchronous
                ? "Agent can continue"
                : question
                  ? "Answer required"
                  : approval
                    ? "Approval required"
                    : "Request"}
            </span>
          </div>
          <p className="request-prompt">
            {questions[0]?.question ||
              p.reason ||
              p.message ||
              "Approval required."}
          </p>
          {questions[0]?.question && (p.reason || p.message) && (
            <p>{p.reason || p.message}</p>
          )}
          {questions.length > 1 && <p>{questions.length} questions</p>}
          {(p.command || r.preview?.command) && (
            <pre>{JSON.stringify(p.command || r.preview.command)}</pre>
          )}
          {p.cwd && <p>{p.cwd}</p>}
          {(p.permissions || r.preview?.changes) && (
            <pre>
              {JSON.stringify(p.permissions || r.preview.changes, null, 2)}
            </pre>
          )}
          {p.url && /^https?:\/\//.test(p.url) && (
            <a href={p.url} target="_blank" rel="noreferrer">
              Open request
            </a>
          )}
        </div>
        <div className="request-actions">
          {question ? (
            <Button
              ref={trigger}
              data-answer={r.id}
              disabled={sending}
              aria-expanded={asynchronous ? open : undefined}
              aria-controls={asynchronous ? `answer-${r.id}` : undefined}
              onClick={() => setOpen(!open)}
            >
              {open && asynchronous ? "Hide" : "Answer"}
            </Button>
          ) : approval ? (
            <>
              <Button
                disabled={sending}
                onClick={() => void post({ decision: "accept" })}
              >
                Approve
              </Button>
              <Button
                disabled={sending}
                onClick={() => void post({ decision: "decline" })}
              >
                Decline
              </Button>
            </>
          ) : (
            <p>Unsupported client request: {r.method}</p>
          )}
        </div>
      </div>
      {asynchronous
        ? open && (
            <div id={`answer-${r.id}`} className="request-inline-answer">
              <AnswerForm
                request={r}
                sending={sending}
                post={post}
                close={close}
                notify={notify}
              />
            </div>
          )
        : question && (
            <Modal
              opened={open}
              onClose={() => {
                if (!sending) close();
              }}
              title="Reply to the agent"
            >
              {open && (
                <AnswerForm
                  request={r}
                  sending={sending}
                  post={post}
                  close={close}
                  notify={notify}
                />
              )}
            </Modal>
          )}
    </div>
  );
}

export default function Requests({ requests, ...props }: Props) {
  return (
    <div id="requests">
      {requests.map((r) => (
        <RequestCard key={`${r.agent}:${r.id}`} request={r} {...props} />
      ))}
    </div>
  );
}
