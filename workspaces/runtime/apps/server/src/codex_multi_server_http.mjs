// A process deadline also bounds DNS resolution and a slow response body.
import {
  MULTI_SERVER_MAX_REQUEST_BYTES,
  MULTI_SERVER_MAX_RESPONSE_BYTES,
} from "./generated/multi_server_transport_limits.mjs";

const MAX_HELPER_STDIN_BYTES = 512 * 1024;
const chunks = [];
let inputSize = 0;
for await (const chunk of process.stdin) {
  inputSize += chunk.length;
  if (inputSize > MAX_HELPER_STDIN_BYTES)
    throw new Error("The request input is too large");
  chunks.push(chunk);
}
const input = JSON.parse(Buffer.concat(chunks).toString("utf8"));
async function exchange() {
  const url = new URL(input.url);
  if (
    url.protocol !== "https:" ||
    url.username ||
    url.password ||
    !Number.isFinite(input.timeout) ||
    input.timeout <= 0 ||
    input.timeout > 120
  ) {
    throw new Error("The transport request is invalid");
  }
  const body = Buffer.from(input.body, "base64");
  if (body.length > MULTI_SERVER_MAX_REQUEST_BYTES)
    throw new Error("The request body is too large");
  const response = await fetch(url, {
    method: input.method,
    headers: input.headers,
    body: body.length ? body : undefined,
    redirect: "manual",
    signal: AbortSignal.timeout(Math.max(1, Math.floor(input.timeout * 1000))),
  });
  const output = [];
  let size = 0;
  for await (const chunk of response.body ?? []) {
    size += chunk.length;
    if (size > MULTI_SERVER_MAX_RESPONSE_BYTES)
      throw Object.assign(new Error("The server response is too large"), {
        code: "response_size",
      });
    output.push(chunk);
  }
  return {
    status: response.status,
    url: response.url,
    body: Buffer.concat(output).toString("base64"),
  };
}
try {
  process.stdout.write(JSON.stringify(await exchange()));
} catch (error) {
  process.stdout.write(
    JSON.stringify({
      failure:
        error.code === "response_size" ? "response_size" : "remote_unavailable",
    }),
  );
}
