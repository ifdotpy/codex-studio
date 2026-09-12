import assert from 'node:assert/strict';
const { saveFile, openFile, revealFile, previewFile, canUseNativeFiles } = await import('../web/src/fileActions.ts');
const calls = [];
globalThis.window = { codexDesktop: {
  saveFile: (value) => { calls.push(['save', value]); return Promise.resolve(false); },
  fileAction: (value) => { calls.push(['file', value]); return Promise.resolve(true); },
} };
assert.equal(canUseNativeFiles(), true);
const bytes = Uint8Array.of(0, 1, 2, 3).subarray(1, 3);
const saving = saveFile({ name: '../report.md', mime: 'text/markdown', data: bytes });
assert.equal(calls.length, 1, 'Native gesture must be consumed before the first await');
assert.equal(calls[0][1].name, 'report.md');
assert.deepEqual([...new Uint8Array(calls[0][1].data)], [1, 2]);
assert.equal(await saving, false, 'Cancellation must not become success');
for (const action of [openFile, revealFile, previewFile])
  await action({ agent: 'a', path: 'report.md', line: 30 });
assert.deepEqual(calls.slice(1).map((row) => row[1]), ['open', 'reveal', 'preview'].map((action) => ({ action, target: { agent: 'a', path: 'report.md' } })));
await assert.rejects(saveFile({ name: '..', data: 'x' }), /file name/);
window.codexDesktop.saveFile = async () => { throw new Error('fixture write failure'); };
await assert.rejects(saveFile({ name: 'report.md', data: 'x' }), /write failure/);
window.codexDesktop = undefined;
assert.equal(canUseNativeFiles(), false);
assert.equal(await openFile({ agent: 'a', path: 'report.md' }), false);
let shared;
Object.defineProperty(globalThis, 'navigator', { configurable: true, value: {
  canShare: ({ files }) => files.length === 1,
  share: async ({ files }) => { shared = files[0]; },
} });
assert.equal(await saveFile({ name: 'report.md', mime: 'text/markdown', data: '# Progress' }), true);
assert.equal(shared.name, 'report.md');
assert.equal(shared.type, 'text/markdown');
assert.equal(await shared.text(), '# Progress');
navigator.share = async () => { throw new DOMException('User cancelled', 'AbortError'); };
assert.equal(await saveFile({ name: 'report.md', data: 'x' }), false);
navigator.share = async () => { throw new Error('fixture sharing failure'); };
await assert.rejects(saveFile({ name: 'report.md', data: 'x' }), /sharing failure/);
const link = { click() { calls.push(['browser-save', this.download]); }, remove() {} };
globalThis.document = { createElement: () => link, body: { append() {} } };
navigator.canShare = () => false;
const originalTimeout = globalThis.setTimeout;
let release;
globalThis.setTimeout = (callback, delay) => { assert.equal(delay, 60000); release = callback; return 0; };
try {
  assert.equal(await saveFile({ name: 'report.md', data: 'x' }), true);
  assert.deepEqual(calls.at(-1), ['browser-save', 'report.md']);
  assert.match(link.href, /^blob:/);
  release();
} finally {
  globalThis.setTimeout = originalTimeout;
}
console.log('Shared file actions PASS: native synchronous gesture, exact bytes, cancellation/errors, target identity, PWA share, explicit browser save fallback');
