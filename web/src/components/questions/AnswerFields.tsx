import { TextInput, Textarea, UnstyledButton } from "@mantine/core";

export type AnswerQuestion = {
  id: string;
  question: string;
  isSecret?: boolean;
  options?: { label: string; description?: string }[];
};

export type AnswerFieldsProps = {
  questions: AnswerQuestion[];
  values: Record<string, string>;
  disabled?: boolean;
  onChange: (id: string, value: string) => void;
};

export default function AnswerFields({
  questions,
  values,
  disabled = false,
  onChange,
}: AnswerFieldsProps) {
  return (
    <fieldset disabled={disabled} className="request-answer-fields">
      {questions.map((question, index) => (
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
              {question.options.map((option) => (
                <UnstyledButton
                  key={option.label}
                  type="button"
                  className="request-answer-option"
                  aria-pressed={values[question.id] === option.label}
                  onClick={() => onChange(question.id, option.label)}
                >
                  <span>{option.label}</span>
                  {option.description && <small>{option.description}</small>}
                </UnstyledButton>
              ))}
            </div>
          ) : null}
          {question.isSecret ? (
            <TextInput
              aria-label={question.question}
              autoFocus={index === 0}
              type="password"
              value={values[question.id] || ""}
              onChange={(event) => onChange(question.id, event.target.value)}
            />
          ) : (
            <Textarea
              aria-label={question.question}
              autoFocus={index === 0}
              value={values[question.id] || ""}
              autosize
              minRows={2}
              maxRows={8}
              onChange={(event) => onChange(question.id, event.target.value)}
              placeholder={
                question.options?.length
                  ? "Or write your own answer"
                  : "Your answer"
              }
            />
          )}
        </div>
      ))}
    </fieldset>
  );
}
