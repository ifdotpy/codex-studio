import type { AskUserQuestionInput } from "@anthropic-ai/claude-agent-sdk/sdk-tools";
import { z } from "zod";

type Question = AskUserQuestionInput["questions"][number];
type Option = Question["options"][number];
const optionFields = {
  label: z.string(),
  description: z.string(),
  preview: z.string().optional(),
} satisfies { [K in keyof Option]-?: z.ZodType<Option[K]> };
const option = z
  .object(optionFields)
  .transform(({ label, description, preview }) => ({
    label,
    description,
    ...(preview === undefined ? {} : { preview }),
  })) satisfies z.ZodType<Question["options"][number]>;
const choicesSchema = z.union([
  z.tuple([option, option]),
  z.tuple([option, option, option]),
  z.tuple([option, option, option, option]),
]) satisfies z.ZodType<Question["options"]>;
const choices: [Question["options"]] extends [z.output<typeof choicesSchema>]
  ? typeof choicesSchema
  : never = choicesSchema;
const questionFields = {
  question: z.string(),
  header: z.string(),
  options: choices,
  multiSelect: z.boolean(),
} satisfies { [K in keyof Question]-?: z.ZodType<Question[K]> };
const question = z.object(questionFields) satisfies z.ZodType<Question>;
const questionsSchema = z.union([
  z.tuple([question]),
  z.tuple([question, question]),
  z.tuple([question, question, question]),
  z.tuple([question, question, question, question]),
]) satisfies z.ZodType<AskUserQuestionInput["questions"]>;
const questions: [AskUserQuestionInput["questions"]] extends [
  z.output<typeof questionsSchema>,
]
  ? typeof questionsSchema
  : never = questionsSchema;

// No runtime schema is exported by the pinned SDK tools package. Construct the
// consumed parser against its exact tuple/field owners, preserving omission.
export function parseUserQuestions(
  value: unknown,
): AskUserQuestionInput["questions"] | null {
  const result = questions.safeParse(value);
  return result.success ? result.data : null;
}
