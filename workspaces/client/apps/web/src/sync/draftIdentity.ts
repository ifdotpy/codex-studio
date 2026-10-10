export function draftVersionId(
  device: string,
  writer: string,
  session: string,
) {
  return `${device}:${writer}:${session}`;
}
