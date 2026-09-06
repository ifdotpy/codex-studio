export interface CodexDesktop {
  readonly platform: string;
  pickDirectory(): Promise<string | null>;
  pickFiles(): Promise<
    Array<{ name: string; path: string; mime: string; data: string }>
  >;
  revealPath(path: string): Promise<void>;
  openExternal(url: string): Promise<void>;
  setNotifications(enabled: boolean): Promise<boolean>;
  notify(value: { title: string; body: string }): Promise<boolean>;
}
declare global {
  interface Window {
    codexDesktop?: CodexDesktop;
  }
}
