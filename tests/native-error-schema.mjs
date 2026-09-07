// Compare UI coverage to the installed native protocol without model requests.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { nativeErrorHints, nativeErrorView } from '../web/src/nativeErrors.ts';
const dir = mkdtempSync(join(tmpdir(), 'studio-error-schema-'));
try {
  execFileSync(process.env.CODEX_BIN || 'codex', ['app-server', 'generate-json-schema', '--experimental', '--out', dir]);
  const schema = JSON.parse(readFileSync(join(dir, 'v2/ErrorNotification.json'), 'utf8'));
  const codes = schema.definitions.CodexErrorInfo.oneOf.flatMap(v => v.enum || Object.keys(v.properties));
  assert.deepEqual([...codes].sort(), Object.keys(nativeErrorHints).sort());
  for (const code of codes) {
    for (const info of [code, { [code]: { httpStatusCode: 503 } }]) {
      const view = nativeErrorView({ message: 'Native message', codexErrorInfo: info, additionalDetails: 'Cause' });
      assert.equal(view.message, 'Native message');
      assert.ok(view.hint.length > 0);
      assert.ok(view.details.includes('Cause'));
    }
  }
  assert.equal(nativeErrorView('{"message":"RPC rejected","code":-32600}').message, 'RPC rejected');
  assert.equal(nativeErrorView('ordinary failure').message, 'ordinary failure');
  assert.ok(nativeErrorView({ message: 'New variant', codexErrorInfo: 'futureCode' }).hint);
  console.log(`PASS: ${codes.length} installed native error variants, structured and plain errors, unknown variant fallback`);
} finally { rmSync(dir, { recursive: true, force: true }); }
