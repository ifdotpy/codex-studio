import { Checkbox, TextInput, Textarea, UnstyledButton } from "@mantine/core";

export type AnswerQuestion = {
  id: string;
  question: string;
  isSecret?: boolean;
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
  const { selected, other } = answerParts(value);
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
  return (
    <fieldset disabled={disabled} className="request-answer-fields">
      {questions.map((question, index) => {
        const value = answerParts(values[question.id]);
        return (
          <div className="answer-field" key={question.id}>
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
                {question.options.map((option) =>
                  question.multiSelect ? (
                    <Checkbox
                      key={option.label}
                      className="request-answer-option request-answer-checkbox"
                      label={option.label}
                      description={option.description}
                      checked={value.selected.includes(option.label)}
                      onChange={(event) =>
                        onChange(question.id, {
                          ...value,
                          selected: event.currentTarget.checked
                            ? [...value.selected, option.label]
                            : value.selected.filter(
                                (label) => label !== option.label,
                              ),
                        })
                      }
                    />
                  ) : (
                    <UnstyledButton
                      key={option.label}
                      type="button"
                      className="request-answer-option"
                      aria-pressed={value.selected.includes(option.label)}
                      onClick={() =>
                        onChange(question.id, {
                          selected: [option.label],
                          other: "",
                        })
                      }
                    >
                      <span>{option.label}</span>
                      {option.description && (
                        <small>{option.description}</small>
                      )}
                    </UnstyledButton>
                  ),
                )}
              </div>
            ) : null}
            {question.isSecret ? (
              <TextInput
                aria-label={question.question}
                autoFocus={index === 0}
                type="password"
                value={value.other}
                onChange={(event) => onChange(question.id, event.target.value)}
              />
            ) : (
              <Textarea
                aria-label={question.question}
                autoFocus={index === 0}
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
                  question.options?.length
                    ? "Or write your own answer"
                    : "Your answer"
                }
              />
            )}
          </div>
        );
      })}
    </fieldset>
  );
}
