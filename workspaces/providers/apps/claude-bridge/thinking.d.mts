export type ThinkingModel = {
  value?: string;
  model?: string;
  resolvedModel?: string;
  description?: string;
};
export function requiresThinking(
  model: string,
  models?: ThinkingModel[],
): boolean;
export function thinkingFlag(
  model: string,
  models: ThinkingModel[],
  saved?: boolean,
  live?: boolean,
): { alwaysThinkingEnabled?: boolean | null };
