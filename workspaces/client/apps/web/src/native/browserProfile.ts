export type EncodedProfileValue =
  | ["undefined"]
  | ["value", null | string | boolean | number]
  | ["number", string]
  | ["date", string]
  | ["bytes", string]
  | ["blob", string, EncodedProfileValue]
  | ["file", string, string, number, EncodedProfileValue]
  | ["view", string, EncodedProfileValue]
  | ["array", EncodedProfileValue[]]
  | ["object", [string, EncodedProfileValue][]];
export type BrowserProfileRecord = [EncodedProfileValue, EncodedProfileValue];
export type BrowserProfileChunk = {
  sha256: string;
  bytes: number;
  records: number;
  owner?: string;
};
export type BrowserProfileStore = {
  name: string;
  keyPath: string | string[] | null;
  autoIncrement: boolean;
  nextKey?: number;
  indexes: {
    name: string;
    keyPath: string | string[];
    unique: boolean;
    multiEntry: boolean;
  }[];
  records?: BrowserProfileRecord[];
  chunks?: BrowserProfileChunk[];
};
export type BrowserProfileDatabase = {
  name: string;
  version: number;
  stores: BrowserProfileStore[];
};
export type BrowserProfileSnapshot = {
  version: 1 | 2;
  origin?: string;
  topLevelOrigin?: string;
  preferences: Record<string, string>;
  databases: BrowserProfileDatabase[];
};

/** Archive envelope shared by the offline exporter and native import checks. */
export type BrowserProfileArchive = {
  version: 1 | 2;
  parts?: string;
  id: string;
  profile?: string;
  created?: number;
  snapshots: BrowserProfileSnapshot[];
  owners: Record<string, string>;
};
