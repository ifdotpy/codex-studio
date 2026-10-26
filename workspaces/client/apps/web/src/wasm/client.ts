import init, { parse_markdown } from "./generated/client";
import wasmURL from "./generated/client_bg.wasm?url";

let ready: Promise<unknown> | undefined;
export async function parseMarkdown(
  text: string,
  streaming: boolean,
): Promise<unknown> {
  ready ??= init({ module_or_path: wasmURL }).catch((error: unknown) => {
    ready = undefined;
    throw error;
  });
  await ready;
  return JSON.parse(parse_markdown(text, streaming));
}
