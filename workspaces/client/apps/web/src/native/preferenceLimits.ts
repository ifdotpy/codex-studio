import contract from "../../../../contracts/preference-storage.json?raw";

// UTF-8 bytes including the native browser-owner prefix.
export const maxPreferenceKeyBytes: number = JSON.parse(contract).maxKeyBytes;
