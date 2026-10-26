import type { components } from "../../generated/api";
import type { Message } from "../../types";

/** One row of `GET /api/questions`, owned by the server's QuestionHistory contract. */
export type QuestionHistoryEntry = components["schemas"]["AgentRequest"];
type RequestDto = components["schemas"]["RequestEntityDto"];

export interface QuestionChoice {
  label: string;
  description?: string;
}

export interface CardQuestion {
  id: string;
  header?: string;
  question: string;
  options: QuestionChoice[];
  multiSelect: boolean;
  isSecret: boolean;
  /** The user's answers in provider order; undefined while no answer is known. */
  answers?: string[] | undefined;
}

export type QuestionCardState =
  | "waiting"
  | "answered"
  | "uncertain"
  | "closed"
  | "unanswered"
  | "failed";

export interface QuestionCardModel {
  /** `tool`: Claude AskUserQuestion item. `message`: Codex question in an agent message. */
  source: "tool" | "message";
  state: QuestionCardState;
  /** The agent keeps working while the question waits (Codex asynchronous question). */
  asynchronous: boolean;
  questions: CardQuestion[];
  /** Provider result text when it cannot be split into per-question answers. */
  resultText?: string;
  /** The pending request that the existing answer form answers. */
  requestId?: string;
}

type JsonObject = Record<string, unknown>;

const isObject = (value: unknown): value is JsonObject =>
  typeof value === "object" && value !== null && !Array.isArray(value);
const text = (value: unknown): string | undefined =>
  typeof value === "string" ? value : undefined;

const QUESTION_TOOL = "AskUserQuestion";
const parsed = new WeakMap<
  Message,
  { text: string; value?: JsonObject | undefined | undefined }
>();

/** The stored provider item of a Claude AskUserQuestion call, or undefined. */
function questionToolPayload(item: Message): JsonObject | undefined {
  if (!["tool", "output"].includes(item.role)) return undefined;
  const prior = parsed.get(item);
  if (prior?.text === item.text) return prior.value;
  let value: JsonObject | undefined;
  // Cheap prefilter: most tool rows are not questions.
  if (item.text.includes(QUESTION_TOOL)) {
    try {
      const candidate: unknown = JSON.parse(item.text);
      if (
        isObject(candidate) &&
        candidate.type === "mcpToolCall" &&
        candidate.tool === QUESTION_TOOL &&
        isObject(candidate.arguments) &&
        toolQuestions(candidate.arguments).length > 0
      )
        value = candidate;
    } catch {
      /* Clipped or plain output keeps the generic tool view. */
    }
  }
  parsed.set(item, { text: item.text, value });
  return value;
}

function choices(value: unknown): QuestionChoice[] {
  if (!Array.isArray(value)) return [];
  return value.flatMap((entry) => {
    if (typeof entry === "string") return [{ label: entry }];
    if (!isObject(entry) || !text(entry.label)) return [];
    const description = text(entry.description);
    return [
      { label: entry.label as string, ...(description ? { description } : {}) },
    ];
  });
}

function toolQuestions(args: JsonObject): CardQuestion[] {
  const questions = Array.isArray(args.questions) ? args.questions : [];
  const result = questions.flatMap((entry, index) => {
    if (!isObject(entry) || !text(entry.question)) return [];
    const header = text(entry.header);
    return [
      {
        id: String(index),
        ...(header ? { header } : {}),
        question: entry.question as string,
        options: choices(entry.options),
        multiSelect: entry.multiSelect === true,
        isSecret: false,
      },
    ];
  });
  // A partly malformed call keeps the generic view instead of hiding questions.
  return result.length === questions.length ? result : [];
}

/** True for a Claude AskUserQuestion item that renders as a question card. */
export function isQuestionToolItem(item: Message): boolean {
  return questionToolPayload(item) !== undefined;
}

function resultText(value: unknown): string | undefined {
  const content = isObject(value) ? value.content : undefined;
  if (typeof content === "string") return content;
  if (Array.isArray(content))
    return content
      .map((part) => (isObject(part) ? text(part.text) || "" : ""))
      .join("\n");
  return undefined;
}

/**
 * Split Claude Code's AskUserQuestion result into one answer per question.
 * The tool returns `"<question>"="<answer>", ...` followed by a sentence for
 * the model. The question texts are known, so each answer runs to the next
 * known question marker, and the last answer to the final `". `.
 */
export function parseToolAnswers(
  content: string,
  questions: string[],
): string[] | undefined {
  const markers: { start: number; end: number }[] = [];
  let cursor = 0;
  for (const question of questions) {
    const marker = `"${question}"="`;
    const start = content.indexOf(marker, cursor);
    if (start < 0) return undefined;
    markers.push({ start, end: start + marker.length });
    cursor = start + marker.length;
  }
  return markers.map((marker, index) => {
    const next = markers[index + 1];
    if (next) {
      const value = content.slice(marker.end, next.start);
      return value.endsWith('", ') ? value.slice(0, -3) : value.trimEnd();
    }
    const rest = content.slice(marker.end);
    const close = rest.lastIndexOf('". ');
    if (close >= 0) return rest.slice(0, close);
    const quote = rest.lastIndexOf('"');
    return quote >= 0 ? rest.slice(0, quote) : rest;
  });
}

/**
 * Claude joins multiple choices with ", ". Rebuild the choices from the known
 * option labels, so a label that itself contains ", " stays whole; any text
 * that is not a label is the user's own answer.
 */
export function splitJoinedAnswer(
  answer: string,
  question: Pick<CardQuestion, "options" | "multiSelect">,
): string[] {
  const labels = new Set(question.options.map((option) => option.label));
  if (!question.multiSelect || labels.has(answer)) return [answer];
  const parts = answer.split(", ");
  const result: string[] = [];
  let other: string[] = [];
  for (let index = 0; index < parts.length;) {
    let matched = 0;
    for (let end = parts.length; end > index; end--)
      if (labels.has(parts.slice(index, end).join(", "))) {
        matched = end - index;
        break;
      }
    if (!matched) {
      const part = parts[index];
      if (part === undefined) throw new Error("Missing answer part");
      other.push(part);
      index++;
      continue;
    }
    if (other.length) result.push(other.join(", "));
    other = [];
    result.push(parts.slice(index, index + matched).join(", "));
    index += matched;
  }
  if (other.length) result.push(other.join(", "));
  return result;
}

/** Which offered options the user chose, and any answer text of their own. */
export function answerParts(question: CardQuestion): {
  chosen: Set<string>;
  own: string[];
} {
  const labels = new Set(question.options.map((option) => option.label));
  const chosen = new Set<string>();
  const own: string[] = [];
  for (const answer of question.answers || []) {
    if (labels.has(answer)) chosen.add(answer);
    else if (answer.trim()) own.push(answer);
  }
  return { chosen, own };
}

function historyAnswers(
  entry: QuestionHistoryEntry,
  question: CardQuestion,
  index: number,
): string[] | undefined {
  const history = entry.answerHistory || [];
  const row =
    history.find((answer) => answer.question === question.question) ||
    (history.length === entry.questions?.length ? history[index] : undefined);
  if (!row) return undefined;
  if (row.isSecret) return undefined;
  const value = row.answer;
  if (Array.isArray(value))
    return value.filter(
      (answer): answer is string => typeof answer === "string",
    );
  if (typeof value === "string") return [value];
  return value == null ? undefined : [JSON.stringify(value)];
}

function historyState(entry: QuestionHistoryEntry): QuestionCardState {
  if (entry.status === "pending" || entry.status === "blocked")
    return "waiting";
  if (entry.status === "answered") return "answered";
  if (entry.status === "answering" || entry.status === "uncertain")
    return "uncertain";
  if (entry.status === "failed") return "failed";
  return "unanswered";
}

function toolState(item: Message, payload: JsonObject): QuestionCardState {
  const status = item.toolStatus || text(payload.status) || "";
  if (["running", "inProgress", "pending", "approval"].includes(status))
    return "waiting";
  if (status === "completed") return "answered";
  if (status === "failed") return "failed";
  return "unanswered";
}

/** A live request that waits for the user's answer to an agent question. */
export const isPendingQuestionRequest = (request: RequestDto) =>
  request.status === "pending" &&
  ["item/tool/requestUserInput", "agent/asyncQuestion"].includes(
    request.method || "",
  );

/** Live pending question requests, keyed by agent and provider item id. */
export function pendingQuestionRequests(requests: RequestDto[]) {
  const result = new Map<string, string>();
  for (const request of requests) {
    if (!isPendingQuestionRequest(request) || !request.agent) continue;
    const params = isObject(request.params) ? request.params : {};
    const item = text(params.itemId);
    if (item) result.set(questionKey(request.agent, item), request.id);
  }
  return result;
}

export const questionKey = (agent: string, item: string) =>
  `${agent}\u0000${item}`;

/** Index question history rows by the transcript item that asked them. */
export function questionHistoryIndex(entries: QuestionHistoryEntry[]) {
  const result = new Map<string, QuestionHistoryEntry>();
  for (const entry of entries)
    if (entry.itemId) result.set(questionKey(entry.agent, entry.itemId), entry);
  return result;
}

export interface QuestionLookup {
  agentId?: string | undefined;
  history: Map<string, QuestionHistoryEntry>;
  /** Live pending request ids by question key; empty when unknown. */
  pending: Map<string, string>;
  /** Live pending request ids, used to confirm a pending history row. */
  pendingIds: Set<string>;
}

/** The question card for a transcript item, or undefined for an ordinary row. */
export function questionCard(
  item: Message,
  lookup?: QuestionLookup,
): QuestionCardModel | undefined {
  const agent = lookup?.agentId;
  const payload = questionToolPayload(item);
  if (payload) {
    const questions = toolQuestions(payload.arguments as JsonObject);
    const nativeId = text(payload.id);
    const key = agent && nativeId ? questionKey(agent, nativeId) : undefined;
    const entry = key ? lookup?.history.get(key) : undefined;
    let state = toolState(item, payload);
    const result = resultText(payload.result);
    let exact = false;
    if (entry?.answerHistory?.length) {
      questions.forEach((question, index) => {
        question.answers = historyAnswers(entry, question, index);
      });
      exact = questions.some((question) => question.answers);
    }
    if (!exact && result && state === "answered") {
      const answers = parseToolAnswers(
        result,
        questions.map((question) => question.question),
      );
      questions.forEach((question, index) => {
        const answer = answers?.[index];
        if (answer !== undefined)
          question.answers = splitJoinedAnswer(answer, question);
      });
      exact = !!answers;
    }
    const requestId =
      state === "waiting" && key
        ? lookup?.pending.get(key) ||
          (entry && lookup?.pendingIds.has(entry.id) ? entry.id : undefined)
        : undefined;
    if (entry && state !== "waiting" && !exact && entry.status === "uncertain")
      state = "uncertain";
    // A finished call without recognizable answers, such as a closed question.
    else if (state === "answered" && !exact) state = "closed";
    return {
      source: "tool",
      state,
      asynchronous: false,
      questions,
      ...(!exact && result ? { resultText: result } : {}),
      ...(requestId ? { requestId } : {}),
    };
  }
  if (item.role !== "assistant" || !agent || !lookup) return undefined;
  const prefix = `${agent}:`;
  if (!item.id.startsWith(prefix)) return undefined;
  const entry = lookup.history.get(
    questionKey(agent, item.id.slice(prefix.length)),
  );
  if (!entry || entry.method !== "agent/asyncQuestion") return undefined;
  const pending = lookup.pendingIds.has(entry.id);
  const questions = (entry.questions || []).map((field, index) => {
    const question: CardQuestion = {
      id: field.id,
      ...(field.header ? { header: field.header } : {}),
      question: field.question,
      options: (field.options || []).map((option) => ({
        label: option.label,
        ...(option.description ? { description: option.description } : {}),
      })),
      multiSelect: field.multiSelect === true,
      isSecret: field.isSecret,
    };
    question.answers = historyAnswers(entry, question, index);
    return question;
  });
  if (!questions.length) return undefined;
  return {
    source: "message",
    state: historyState(entry),
    asynchronous: true,
    questions,
    ...(pending ? { requestId: entry.id } : {}),
  };
}

/** Text that changes when a question may have changed state; drives history refresh. */
export function questionRefreshKey(
  items: Message[],
  requests: RequestDto[],
): string {
  const tools = items
    .filter(isQuestionToolItem)
    .map((item) => `${item.id}:${item.toolStatus || ""}`);
  const pending = requests
    .filter(isPendingQuestionRequest)
    .map((request) => request.id)
    .sort();
  return JSON.stringify([tools, pending]);
}
