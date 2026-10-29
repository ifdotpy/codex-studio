import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";
import test from "node:test";

const binary = path.resolve(
  process.env.CARGO_TARGET_DIR ?? "target",
  "debug/studio-runtime-server",
);

function openStore(state: string, interrupted = false): Promise<void> {
  return new Promise((resolve, reject) => {
    let output = "";
    const child = spawn(binary, ["--port", "0"], {
      env: {
        PATH: process.env.PATH,
        HOME: state,
        CODEX_AGENTS_STATE_DIR: state,
        CODEX_STUDIO_STARTUP_TRACE: "1",
      },
      stdio: ["ignore", "ignore", "pipe"],
    });
    const timeout = setTimeout(() => child.kill("SIGKILL"), 30_000);
    child.stderr.on("data", (chunk: Buffer) => {
      output += chunk.toString();
      if (!interrupted && output.includes("store-open-completed"))
        child.kill("SIGTERM");
    });
    child.on("error", reject);
    child.on("exit", (code) => {
      clearTimeout(timeout);
      if (
        interrupted
          ? code !== 0 && output.includes("injected interruption")
          : output.includes("store-open-completed")
      )
        resolve();
      else reject(new Error(`server exit ${code}: ${output}`));
    });
  });
}

test("receipts migration resumes committed batches on server restart", async (t) => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), "receipts-resume-"));
  t.after(() => fs.rmSync(directory, { recursive: true, force: true }));
  const interrupted = path.join(directory, "interrupted");
  const uninterrupted = path.join(directory, "uninterrupted");
  fs.mkdirSync(interrupted);
  await openStore(interrupted);
  const db = new DatabaseSync(path.join(interrupted, "canvas.sqlite3"));
  db.exec(
    "DELETE FROM runtime_payload_migrations WHERE name LIKE 'receipts:%'; CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT NOT NULL,result TEXT NOT NULL)",
  );
  const insert = db.prepare(
    "INSERT INTO runtime_operation_receipts VALUES(?,?,?)",
  );
  db.exec("BEGIN");
  for (let index = 0; index < 1001; index++)
    insert.run(
      `queue:old-${index}`,
      "signature",
      '{"status":"updated","revision":"original-revision","capabilities":{"queue":true}}',
    );
  db.exec("COMMIT");
  db.close();
  fs.cpSync(interrupted, uninterrupted, { recursive: true });
  const fault = new DatabaseSync(path.join(interrupted, "canvas.sqlite3"));
  fault.exec(
    "CREATE TRIGGER interrupt_receipts BEFORE DELETE ON runtime_operation_receipts WHEN old.rowid > 500 BEGIN SELECT RAISE(ABORT,'injected interruption'); END",
  );
  fault.close();
  await openStore(interrupted, true);
  const partial = new DatabaseSync(path.join(interrupted, "canvas.sqlite3"));
  assert.equal(
    partial
      .prepare("SELECT count(*) AS count FROM runtime_operation_receipts")
      .get()?.count,
    501,
  );
  assert.equal(
    partial.prepare("SELECT count(*) AS count FROM request_results").get()
      ?.count,
    500,
  );
  partial.exec("DROP TRIGGER interrupt_receipts");
  partial.close();
  await openStore(interrupted);
  await openStore(uninterrupted);
  const counts = [interrupted, uninterrupted].map((state) => {
    const connection = new DatabaseSync(path.join(state, "canvas.sqlite3"));
    const count = connection
      .prepare("SELECT count(*) AS count FROM request_results")
      .get()?.count;
    connection.close();
    return count;
  });
  assert.deepEqual(counts, [1001, 1001]);
});
