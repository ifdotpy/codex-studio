import { memo } from "react";
import { Button } from "@mantine/core";
import { Check, MessageCircleQuestion } from "lucide-react";
import {
  answerParts,
  type QuestionCardModel,
  type QuestionCardState,
} from "./questionCardModel";
import "./question-card.css";

function stateLabel(state: QuestionCardState, asynchronous: boolean) {
  if (state === "waiting")
    return asynchronous
      ? "Waiting for your answer · Agent can continue"
      : "Waiting for your answer";
  if (state === "answered") return "Answered";
  if (state === "uncertain") return "Answer delivery uncertain";
  if (state === "failed") return "Question failed";
  if (state === "closed") return "Closed";
  return "Not answered";
}

/** A question that an agent asked the user, with the options and the answer. */
export default memo(function QuestionCard({
  card,
  messageId,
  messageText,
  onAnswer,
}: {
  card: QuestionCardModel;
  messageId?: string;
  /** Text of the agent message that already shows the question, if any. */
  messageText?: string | undefined;
  onAnswer?: (requestId: string) => void;
}) {
  const shown = messageText?.trim();
  return (
    <section
      className="question-card"
      data-message={card.source === "tool" ? messageId : undefined}
      data-question-state={card.state}
      aria-label="Agent question"
    >
      <header className="question-card-head">
        <MessageCircleQuestion size={16} className="question-card-icon" />
        <strong>
          {card.questions.length > 1
            ? `${card.questions.length} questions`
            : "Question"}
        </strong>
        <span className="question-card-state">
          {stateLabel(card.state, card.asynchronous)}
        </span>
      </header>
      {card.questions.map((question) => {
        const { chosen, own } = answerParts(question);
        return (
          <div className="question-card-item" key={question.id}>
            {(question.header || question.multiSelect) && (
              <div className="question-card-meta">
                {question.header && (
                  <span className="question-card-header">
                    {question.header}
                  </span>
                )}
                {question.multiSelect && (
                  <span className="question-card-mode">Multiple choice</span>
                )}
              </div>
            )}
            {question.question !== shown && (
              <p className="question-card-text">{question.question}</p>
            )}
            {question.options.length > 0 && (
              <ul className="question-card-options">
                {question.options.map((option) => {
                  const selected = chosen.has(option.label);
                  return (
                    <li
                      key={option.label}
                      className="question-card-option"
                      data-chosen={selected || undefined}
                    >
                      <span className="question-card-mark">
                        {selected && <Check size={13} aria-label="Chosen" />}
                      </span>
                      <span className="question-card-choice">
                        <span className="question-card-label">
                          {option.label}
                        </span>
                        {option.description && (
                          <small className="question-card-description">
                            {option.description}
                          </small>
                        )}
                      </span>
                    </li>
                  );
                })}
              </ul>
            )}
            {own.length > 0 && (
              <div className="question-card-own">
                <span>{question.options.length ? "Own answer" : "Answer"}</span>
                <p>{own.join("\n")}</p>
              </div>
            )}
            {question.isSecret && card.state === "answered" && (
              <p className="question-card-hidden">Answer hidden</p>
            )}
          </div>
        );
      })}
      {card.resultText && (
        <div
          className={`question-card-result${card.state === "failed" ? " danger" : ""}`}
        >
          <span>{card.state === "failed" ? "Error" : "Reply"}</span>
          <pre>{card.resultText}</pre>
        </div>
      )}
      {card.state === "waiting" && card.requestId && onAnswer && (
        <div className="question-card-actions">
          <Button
            size="compact-sm"
            variant="filled"
            onClick={() => onAnswer(card.requestId!)}
          >
            Answer
          </Button>
        </div>
      )}
    </section>
  );
});
