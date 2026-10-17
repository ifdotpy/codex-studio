export function importBrowserProfile(
  snapshot: unknown,
  archiveId: string,
  readChunk?: (chunk: {
    sha256: string;
    bytes: number;
    records: number;
  }) => Promise<unknown>,
): Promise<void>;
