import { app, BrowserWindow } from "electron";
import * as credentials from "./server-credentials.ts";

declare global {
  var syncTestCredentials: typeof credentials;
}
globalThis.syncTestCredentials = credentials;
const profile = process.argv[2];
if (!profile) throw new Error("Supply the fixture profile.");
app.setPath("userData", profile);
app.whenReady().then(() => {
  const window = new BrowserWindow({ show: false });
  void window.loadURL("about:blank");
});
