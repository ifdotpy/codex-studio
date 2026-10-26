import type { SpeechProgress } from "../speech.ts";
for (const completed of [0, 2]) {
  const progress = { completed, total: 2 } satisfies SpeechProgress;
  process.stderr.write(
    JSON.stringify({ type: "progress", ...progress }) + "\n",
  );
}
console.log(JSON.stringify({ text: "ok" }));
