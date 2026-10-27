import type { MarkdownBlock } from "../generated/rustOwned";
import init, { parse_markdown, sha256 as hashSha256 } from "./generated/client";
import wasmURL from "./generated/client_bg.wasm?url";

let ready: Promise<unknown> | undefined;
async function initialize() {
  ready ??= init({ module_or_path: wasmURL }).catch((error: unknown) => {
    ready = undefined;
    throw error;
  });
  await ready;
}

export async function sha256(bytes: Uint8Array): Promise<Uint8Array> {
  await initialize();
  return hashSha256(bytes);
}

export async function parseMarkdown(
  text: string,
  streaming: boolean,
): Promise<MarkdownBlock[]> {
  await initialize();
  return JSON.parse(parse_markdown(text, streaming));
}
