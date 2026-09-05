import { Button, Modal, NativeSelect, TextInput } from "@mantine/core";
import { MessageCircleQuestion } from "lucide-react";
import { useState } from "react";
import { api, errorText } from "../api";
import type { Json, Agent } from "../types";
export default function Requests({
  requests,
  agents,
  refresh,
  notify,
}: {
  requests: Json[];
  agents: Agent[];
  refresh: () => Promise<void>;
  notify: (s: string) => void;
}) {
  const [answer, setAnswer] = useState<Json | null>(null),
    [values, setValues] = useState<Record<string, string>>({}),
    [sending, setSending] = useState(false);
  const post = async (id: string, body: Json) => {
    setSending(true);
    try {
      await api("/api/answer", { id, ...body });
      setAnswer(null);
      await refresh();
    } catch (e) {
      notify(errorText(e));
    } finally {
      setSending(false);
    }
  };
  const questions =
    answer?.params.questions ||
    Object.entries(answer?.params.requestedSchema?.properties || {}).map(
      ([id, p]) => ({
        id,
        question: (p as Json).title || id,
        options: (p as Json).enum?.map((label: string) => ({ label })),
      }),
    );
  const submit = (e: React.FormEvent) => {
    e.preventDefault();
    if (!answer) return;
    try {
      let body: Json;
      if (
        ["item/tool/requestUserInput", "agent/asyncQuestion"].includes(
          answer.method,
        )
      )
        body = {
          answers: Object.fromEntries(
            questions.map((q: Json) => [
              q.id,
              { answers: [values[q.id] || ""] },
            ]),
          ),
        };
      else {
        const content: Json = {};
        for (const [key, p] of Object.entries(
          answer.params.requestedSchema.properties,
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
      void post(answer.id, body);
    } catch (e) {
      notify(errorText(e));
    }
  };
  return (
    <div id="requests">
      {requests.map((r) => {
        const p = r.params || {},
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
        return (
          <div className="request" key={r.id}>
            <MessageCircleQuestion size={18} className="request-icon" />
            <div className="request-copy">
              <strong>
                {agents.find((a) => a.id === r.agent)?.name || "Codex"}
              </strong>
              <p>
                {p.reason ||
                  p.message ||
                  (question
                    ? "The agent has a question."
                    : "Approval required.")}
              </p>
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
                  data-answer={r.id}
                  onClick={() => {
                    setAnswer(r);
                    setValues({});
                  }}
                >
                  Answer
                </Button>
              ) : approval ? (
                <>
                  <Button
                    disabled={sending}
                    onClick={() => void post(r.id, { decision: "accept" })}
                  >
                    Approve
                  </Button>
                  <Button
                    disabled={sending}
                    onClick={() => void post(r.id, { decision: "decline" })}
                  >
                    Decline
                  </Button>
                </>
              ) : (
                <p>Unsupported client request: {r.method}</p>
              )}
            </div>
          </div>
        );
      })}
      <Modal
        opened={!!answer}
        onClose={() => setAnswer(null)}
        title="Reply to the agent"
      >
        <form id="answer-form" onSubmit={submit}>
          <div id="answer-fields">
            {questions.map((q: Json) => (
              <div className="answer-field" key={q.id}>
                <p>{q.question}</p>
                {q.options?.length > 0 && (
                  <NativeSelect
                    aria-label={`${q.question} options`}
                    value={values[q.id] || ""}
                    onChange={(e) =>
                      setValues({ ...values, [q.id]: e.target.value })
                    }
                  >
                    <option value="">Choose an answer</option>
                    {q.options.map((o: Json) => (
                      <option key={o.label} value={o.label}>
                        {o.label}
                        {o.description ? " · " + o.description : ""}
                      </option>
                    ))}
                  </NativeSelect>
                )}
                <TextInput
                  aria-label={q.question}
                  type={q.isSecret ? "password" : "text"}
                  value={values[q.id] || ""}
                  onChange={(e) =>
                    setValues({ ...values, [q.id]: e.target.value })
                  }
                  placeholder="Your answer"
                />
              </div>
            ))}
          </div>
          <Button
            type="submit"
            variant="filled"
            color="indigo"
            disabled={sending}
          >
            Send answer
          </Button>
        </form>
      </Modal>
    </div>
  );
}
