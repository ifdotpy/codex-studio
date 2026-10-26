import { readFile } from "node:fs/promises";
import { expect, it } from "vitest";
import { transcriptMessages } from "../hooks/presentation";
import type { TranscriptWireItem } from "../types";

it("matches the native transcript view display vectors", async () => {
  const fixture = JSON.parse(
    await readFile(
      new URL(
        "../../../../packages/client-core/tests/fixtures/transcript-presentation.json",
        import.meta.url,
      ),
      "utf8",
    ),
  ) as {
    fields: string[];
    cases: Array<{
      id: string;
      source: TranscriptWireItem[];
      expected: unknown;
    }>;
  };
  for (const sample of fixture.cases) {
    const displayed = transcriptMessages(sample.source, sample.id).map(
      (message) =>
        Object.fromEntries(
          fixture.fields
            .filter(
              (key) => (message as Record<string, unknown>)[key] !== undefined,
            )
            .map((key) => [key, (message as Record<string, unknown>)[key]]),
        ),
    );
    expect(displayed).toEqual(sample.expected);
  }
});
