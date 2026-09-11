export interface CodexDesktop {
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
  setNotifications(enabled: boolean): Promise<boolean>;
  getNotifications(): Promise<boolean>;
  onNavigate(
    callback: (target: {
      agentId: string;
      section: "messages";
      itemId?: string;
    }) => void,
  ): () => void;
  notify(value: {
    title: string;
    body: string;
    target: { agentId: string; section: "messages"; itemId?: string };
  }): Promise<boolean>;
}
declare global {
  interface Window {
    codexDesktop?: CodexDesktop;
  }
}
