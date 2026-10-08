import { Checkbox, TextInput, Textarea, UnstyledButton } from "@mantine/core";
import { useEffect, useRef, useState } from "react";

export type AnswerQuestion = {
  id: string;
  question: string;
  isSecret?: boolean;
  isOther?: boolean;
  multiSelect?: boolean;
  options?: { label: string; description?: string }[];
};

export type AnswerValue = string | { selected: string[]; other: string };
export type AnswerValues = Record<string, AnswerValue>;

export function answerParts(value: AnswerValue | undefined) {
  return typeof value === "string"
    ? { selected: [], other: value }
    : value || { selected: [], other: "" };
}

export function answerList(
  question: AnswerQuestion,
  value: AnswerValue | undefined,
) {
  const { selected, other: draftOther } = answerParts(value);
  const other = question.options?.length && !question.isOther ? "" : draftOther;
  return question.multiSelect
    ? [...selected, ...(other.trim() ? [other.trim()] : [])]
    : [other.trim() || selected[0] || ""];
}

export type AnswerFieldsProps = {
  questions: AnswerQuestion[];
  values: AnswerValues;
  disabled?: boolean;
  onChange: (id: string, value: AnswerValue) => void;
};

export default function AnswerFields({
  questions,
  values,
  disabled = false,
  onChange,
}: AnswerFieldsProps) {
  const fields = useRef<HTMLFieldSetElement>(null);
  const [activeQuestion, setActiveQuestion] = useState(questions[0]?.id);
  const select = (question: AnswerQuestion, label: string) => {
    const value = answerParts(values[question.id]);
    onChange(
      question.id,
      question.multiSelect
        ? {
            ...value,
            selected: value.selected.includes(label)
              ? value.selected.filter((entry) => entry !== label)
              : [...value.selected, label],
          }
        : { selected: [label], other: "" },
    );
  };
  useEffect(() => {
    const keydown = (event: KeyboardEvent) => {
      const target = event.target;
      if (
        disabled ||
        event.altKey ||
        event.ctrlKey ||
        event.metaKey ||
        event.repeat ||
        !(target instanceof HTMLElement) ||
        target.closest(
          'textarea, input:not([type="checkbox"]):not([type="radio"]), [contenteditable="true"]',
        ) ||
        !fields.current?.closest("form")?.contains(target) ||
        !/^[1-9]$/.test(event.key)
      )
        return;
      const question = questions.find((entry) => entry.id === activeQuestion);
      const option = question?.options?.[Number(event.key) - 1];
      if (!question || !option) return;
      event.preventDefault();
      select(question, option.label);
    };
    document.addEventListener("keydown", keydown);
    return () => document.removeEventListener("keydown", keydown);
  });
  return (
    <fieldset
      ref={fields}
      disabled={disabled}
      className="request-answer-fields"
    >
      {questions.map((question, index) => {
        const value = answerParts(values[question.id]);
        return (
          <div
            className="answer-field"
            key={question.id}
            onFocusCapture={() => setActiveQuestion(question.id)}
            onPointerDown={() => setActiveQuestion(question.id)}
          >
            <p className="request-question-label">
              {questions.length > 1 && <span>{index + 1}. </span>}
              {question.question}
            </p>
            {question.options?.length ? (
              <div
                className="request-answer-options"
                role="group"
                aria-label={`${question.question} options`}
              >
                {question.options.map((option, optionIndex) =>
                  question.multiSelect ? (
                    <Checkbox
                      key={option.label}
                      className="request-answer-option request-answer-checkbox"
                      label={
                        <span className="request-option-copy">
                          {optionIndex < 9 && (
                            <span
                              className="request-option-number"
                              aria-hidden="true"
                            >
                              {optionIndex + 1}
                            </span>
                          )}
                          <span>{option.label}</span>
                        </span>
                      }
                      aria-label={option.label}
                      autoFocus={index === 0 && optionIndex === 0}
                      description={option.description}
                      checked={value.selected.includes(option.label)}
                      onChange={() => select(question, option.label)}
                    />
                  ) : (
                    <UnstyledButton
                      key={option.label}
                      type="button"
                      className="request-answer-option"
                      aria-pressed={value.selected.includes(option.label)}
                      aria-label={option.label}
                      autoFocus={index === 0 && optionIndex === 0}
                      onClick={() => select(question, option.label)}
                    >
                      <span className="request-option-copy">
                        {optionIndex < 9 && (
                          <span
                            className="request-option-number"
                            aria-hidden="true"
                          >
                            {optionIndex + 1}
                          </span>
                        )}
                        <span>{option.label}</span>
                      </span>
                      {option.description && (
                        <small>{option.description}</small>
                      )}
                    </UnstyledButton>
                  ),
                )}
              </div>
            ) : null}
            {(!question.options?.length || question.isOther) &&
              (question.isSecret ? (
                <TextInput
                  aria-label={question.question}
                  autoFocus={index === 0 && !question.options?.length}
                  placeholder={question.options?.length ? "Other" : undefined}
                  type="password"
                  value={value.other}
                  onChange={(event) =>
                    onChange(question.id, event.target.value)
                  }
                />
              ) : (
                <Textarea
                  aria-label={question.question}
                  autoFocus={index === 0 && !question.options?.length}
                  value={value.other}
                  autosize
                  minRows={2}
                  maxRows={8}
                  onChange={(event) =>
                    onChange(question.id, {
                      selected: question.multiSelect ? value.selected : [],
                      other: event.target.value,
                    })
                  }
                  placeholder={
                    question.options?.length ? "Other" : "Your answer"
                  }
                />
              ))}
          </div>
        );
      })}
    </fieldset>
  );
}
