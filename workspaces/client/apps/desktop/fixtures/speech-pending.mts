import type { SpeechProgress } from "../speech.ts";
const progress = { completed: 0, total: 1 } satisfies SpeechProgress;
process.stderr.write(JSON.stringify({ type: "progress", ...progress }) + "\n");
setInterval(() => {}, 1000);
