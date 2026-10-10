export interface CodexDesktop {
  readonly serverViewOrigin?: string;
  serverNativeAction?(
    serverId: string,
    method: string,
    value?: unknown,
  ): Promise<unknown>;
  serverCredentialAction?(
    request: import("../web/src/servers/desktopCredentials").CredentialRequest,
  ): Promise<unknown>;
  onServerStream?(
    callback: (
      chunk: import("../web/src/servers/desktopCredentials").StreamChunk,
    ) => void,
  ): () => void;
  readonly platform: string;
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
    Array<{ name: string; path: string; mime: string; data: string }>
  >;
  revealPath(path: string): Promise<void>;
  openExternal(url: string): Promise<void>;
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
    codexDesktop?: CodexDesktop;
  }
}
