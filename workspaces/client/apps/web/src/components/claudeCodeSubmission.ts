export function codeVerificationPending(
  receipt: {
    status: string;
    codeSubmitted?: boolean | null;
  } | null,
): boolean {
  return receipt?.status === "pending" && receipt.codeSubmitted === true;
}

export function currentClaudeLoginReadMatches(
  currentRequestId: string,
  expectedRequestId: string,
  responseRequestId: string,
  currentReadGeneration: number,
  expectedReadGeneration: number,
): boolean {
  return (
    currentRequestId === expectedRequestId &&
    responseRequestId === expectedRequestId &&
    currentReadGeneration === expectedReadGeneration
  );
}
