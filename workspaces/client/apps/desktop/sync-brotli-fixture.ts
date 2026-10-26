import { app, BrowserWindow } from "electron";
import * as credentials from "./server-credentials.ts";

declare global {
  var syncTestCredentials: typeof credentials;
}
globalThis.syncTestCredentials = credentials;
app.setPath("userData", process.argv[2]);
app.whenReady().then(() => {
  const window = new BrowserWindow({ show: false });
  void window.loadURL("about:blank");
});
