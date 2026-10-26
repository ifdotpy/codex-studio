export type AutomaticPairApproval = {
  localServerId: string;
  serverId: string;
  generation: string;
  inviteRequestId: string;
};
export type PairRequest = {
  origin: string;
  invitation: unknown;
  requestId: string;
};
export type ServerRequest = {
  serverId: string;
  credentialId: string;
  url: string;
  method: string;
  headers?: [string, string][] | undefined;
  body?: ArrayBuffer | undefined;
  requestId?: string | undefined;
};
export type CredentialRequest = { frameOwner?: string | undefined } & (
  | ({ action: "pair" } & PairRequest)
  | ({ action: "pairApproved"; approval: AutomaticPairApproval } & PairRequest)
  | ({ action: "request"; streamId: string } & ServerRequest)
  | ({ action: "summary" } & ServerRequest)
  | { action: "cancel"; serverId: string; streamId: string }
  | { action: "forget"; serverId: string }
  | {
      action: "pairAttempt";
      origin: string;
      serverId: string;
      requestId: string;
      inviteId?: string | undefined;
    }
);
export type CredentialResponse = {
  status: number;
  statusText: string;
  headers: [string, string][];
  body?: ArrayBuffer;
  streamId?: string;
};
export type StreamChunk = {
  serverId: string;
  streamId: string;
  bytes?: ArrayBuffer;
  done?: boolean;
  error?: string;
};
/** The redirect port of a sign-in that another server approved for this window. */
export type SignInForward = {
  server: string;
  bridgeId: string;
  host: "127.0.0.1" | "localhost" | "::1";
  port: number;
  path: string;
  expiresAt: number;
  token: string;
};

export interface DesktopBridge {
  serverViewOrigin?: string | undefined;
  serverCredentialAction?(request: CredentialRequest): Promise<unknown>;
  onServerStream?(callback: (chunk: StreamChunk) => void): () => void;
  serverNativeAction?(
    serverId: string,
    method: string,
    value?: unknown,
  ): Promise<unknown>;
  platform: string;
  requestMicrophone(): Promise<boolean>;
  prepareTranscription(): Promise<string>;
  transcribeAudio(value: {
    id: string;
    permit: string;
    audio: ArrayBuffer;
    locale: string;
  }): Promise<{ text: string; provider: string; onDevice: boolean }>;
  cancelTranscription(id: string): Promise<boolean>;
  onTranscriptionProgress(
    callback: (value: { id: string; completed: number; total: number }) => void,
  ): () => void;
  pickDirectory(): Promise<string | null>;
  pickFiles(): Promise<
    { name: string; path: string; mime: string; data: string }[]
  >;
  revealPath(path: string): Promise<void>;
  saveFile?(value: { name: string; data: ArrayBuffer }): Promise<boolean>;
  fileAction?(value: {
    action: "open" | "reveal" | "preview";
    target: { agent?: string; path?: string; asset?: string };
  }): Promise<boolean>;
  openExternal(url: string): Promise<void>;
  /** Open an approved sign-in link and bring the browser to the front. With `forward`, this
   * Mac first holds the redirect port for the command on the server of the frame. */
  openSignIn?(
    value: string | { url: string; forward: SignInForward },
  ): Promise<void>;
  /** The desktop can hold the redirect port of a sign-in on another server. */
  signInForward?: boolean;
  /** Show the paper key of a new vault in Terminal (layr-desk keys). */
  showVaultKeys(prefix: string): Promise<void>;
  getBackendUpdate?(): Promise<{
    availableBackendBuild: string | null;
    updateRequired: boolean | null;
  }>;
  onNavigate(
    callback: (target: {
      serverId?: string;
      agentId: string;
      section: "messages";
      itemId?: string;
    }) => void,
  ): () => void;
  notify(value: {
    title: string;
    body: string;
    /** Play the system sound (sign-in cards); other alerts are silent. */
    sound?: boolean;
    target: {
      serverId?: string;
      agentId: string;
      section: "messages";
      itemId?: string;
    };
  }): Promise<boolean>;
}
declare global {
  interface Window {
    codexDesktop?: DesktopBridge;
  }
}
