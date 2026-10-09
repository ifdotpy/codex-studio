import { execFileSync } from "node:child_process";
import { dirname, delimiter, join } from "node:path";
import { fileURLToPath } from "node:url";

const repositoryRoot = dirname(dirname(fileURLToPath(import.meta.url)));
const resolver = join(repositoryRoot, "scripts", "codex_python.py");

export default function globalSetup() {
  const environment = { ...process.env };
  const requestedPython =
    environment.CODEX_AGENTS_PYTHON ||
    environment.PYTHON_BIN ||
    environment.PYTHON;
  if (requestedPython) environment.CODEX_AGENTS_PYTHON = requestedPython;

  const bootstrapPython =
    environment.PYTHON_BIN || environment.PYTHON || "python3";
  let resolvedPython;
  try {
    resolvedPython = execFileSync(bootstrapPython, [resolver], {
      cwd: repositoryRoot,
      env: environment,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
    }).trim();
  } catch (error) {
    const detail = String(error.stderr || error.message || error).trim();
    throw new Error(
      `Playwright global setup could not resolve Python with FastAPI, Pydantic, Uvicorn, and HTTPX. ${detail}`,
      { cause: error },
    );
  }
  if (!resolvedPython)
    throw new Error("Playwright global setup resolved an empty Python path.");

  process.env.CODEX_AGENTS_PYTHON = resolvedPython;
  process.env.PYTHON_BIN = resolvedPython;
  process.env.PYTHON = resolvedPython;
  const pythonDirectory = dirname(resolvedPython);
  const pathEntries = (process.env.PATH || "").split(delimiter);
  if (pathEntries[0] !== pythonDirectory)
    process.env.PATH = [pythonDirectory, ...pathEntries].join(delimiter);
  console.log(`Playwright fixture Python ${resolvedPython}`);
}
