export type ServerNativeMethod =
  | "requestMicrophone"
  | "prepareTranscription"
  | "transcribeAudio"
  | "cancelTranscription"
  | "pickDirectory"
  | "pickFiles"
  | "revealPath"
  | "saveFile"
  | "fileAction"
  | "openExternal"
  | "getBackendUpdate";
