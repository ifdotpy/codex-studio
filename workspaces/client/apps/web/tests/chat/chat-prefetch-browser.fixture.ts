import * as client from "../../src/sync/client";
import * as cache from "../../src/sync/transcriptCache";
import type { Subscription } from "rxjs";
import type { ResourceVersion } from "../../src/sync/subscriptions/events";

type Payload = Parameters<
  Parameters<typeof client.subscribeTranscriptProjection>[1]
>[0];
type Harness = {
  client: typeof client;
  cache: typeof cache;
  stops: (() => void)[];
  values: Payload[];
  activeValues: Payload[];
  coldReads: string[];
  jobs: Promise<boolean>[];
  race?: Promise<boolean>;
  filtered?: Promise<boolean>;
  filteredResult?: boolean;
  filterRead: boolean;
  releaseFilter: () => void;
  filteredWrites: number[];
  stopFilteredWrites?: Subscription;
  absentRead: boolean;
  releaseAbsent: () => void;
  staleInsert?: ReturnType<typeof client.persistProjection>;
  stopActive: () => void;
  stop: () => void;
  revisionHints: (ResourceVersion | undefined)[];
  stopHints: () => void;
};
declare global {
  interface Window {
    prefetchBrowser: Harness;
  }
}
window.prefetchBrowser = {
  client,
  cache,
  stops: [],
  values: [],
  activeValues: [],
  coldReads: [],
  jobs: [],
  filterRead: false,
  releaseFilter: () => {},
  filteredWrites: [],
  absentRead: false,
  releaseAbsent: () => {},
  stopActive: () => {},
  stop: () => {},
  revisionHints: [],
  stopHints: () => {},
};
