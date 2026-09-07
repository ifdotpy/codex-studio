const { execFile } = require("node:child_process");
const { promisify } = require("node:util");
const fs = require("node:fs/promises");
const path = require("node:path");
const os = require("node:os");
const run = promisify(execFile);
function validateAudio(value) {
  if (
    !(value instanceof ArrayBuffer) ||
    value.byteLength < 46 ||
    value.byteLength > 64 * 1024 * 1024
  )
    throw Error("Expected a WAV recording of at most 64 MB.");
  const data = Buffer.from(value);
  if (
    data.toString("ascii", 0, 4) !== "RIFF" ||
    data.toString("ascii", 8, 12) !== "WAVE" ||
    data.toString("ascii", 12, 16) !== "fmt " ||
    data.readUInt32LE(16) !== 16 ||
    data.readUInt16LE(20) !== 1 ||
    data.readUInt16LE(22) !== 1 ||
    data.readUInt16LE(34) !== 16 ||
    data.toString("ascii", 36, 40) !== "data" ||
    data.readUInt32LE(40) !== data.length - 44
  )
    throw Error("Expected a mono 16-bit PCM WAV recording.");
  const rate = data.readUInt32LE(24);
  if (
    rate < 8000 ||
    rate > 192000 ||
    data.readUInt32LE(28) !== rate * 2 ||
    data.readUInt16LE(32) !== 2 ||
    data.readUInt32LE(4) !== data.length - 8 ||
    (data.length - 44) % 2 ||
    (data.length - 44) / (rate * 2) > 1801
  )
    throw Error("Invalid WAV recording or duration exceeds 30 minutes.");
  return data;
}
async function transcribe({ audio, locale }, helper) {
  const data = validateAudio(audio);
  if (
    typeof locale !== "string" ||
    !/^[a-zA-Z]{2,3}(?:[-_][a-zA-Z0-9]{2,8}){0,3}$/.test(locale)
  )
    throw Error("Invalid speech language.");
  await fs.access(helper).catch(() => {
    throw Error(
      "The native speech helper is missing. Install the updated Codex Studio app. Your recording is saved.",
    );
  });
  const directory = await fs.mkdtemp(
    path.join(os.tmpdir(), "codex-studio-speech-"),
  );
  try {
    const file = path.join(directory, "recording.wav");
    await fs.writeFile(file, data, { mode: 0o600 });
    let stdout;
    try {
      ({ stdout } = await run(helper, [file, locale], {
        timeout: 75 * 60 * 1000,
        maxBuffer: 2 * 1024 * 1024,
      }));
    } catch (error) {
      try {
        const detail = JSON.parse(error.stdout);
        if (detail.error) throw Error(detail.error);
      } catch (parsed) {
        if (parsed instanceof SyntaxError)
          throw Error(
            "Native transcription failed. Your recording is saved; retry transcription.",
          );
        throw parsed;
      }
    }
    const result = JSON.parse(stdout);
    if (typeof result.text !== "string" || !result.text.trim())
      throw Error(
        result.error || "No speech was recognized. Your recording is saved.",
      );
    return result;
  } finally {
    await fs.rm(directory, { recursive: true, force: true });
  }
}
module.exports = { transcribe, validateAudio };
