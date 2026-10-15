import { nativeRequest } from "./client";

export async function saveNativeFile(
  name: string,
  data: ArrayBuffer,
): Promise<boolean> {
  // Reserve before yielding so the native host can check the initiating event.
  const { transfer } = await nativeRequest<{ transfer: string }>(
    "desktop.saveBegin",
    {
      value: { name, size: data.byteLength },
    },
  );
  try {
    const bytes = new Uint8Array(data);
    for (let offset = 0; offset < bytes.length; offset += 256 * 1024) {
      const chunk = bytes.subarray(offset, offset + 256 * 1024);
      const parts: string[] = [];
      for (let at = 0; at < chunk.length; at += 16384)
        parts.push(String.fromCharCode(...chunk.subarray(at, at + 16384)));
      await nativeRequest("desktop.saveChunk", {
        value: { transfer, offset, base64: btoa(parts.join("")) },
      });
    }
    return await nativeRequest<boolean>("desktop.saveFinish", {
      value: { transfer },
    });
  } catch (error) {
    // Cancelling only discards this transfer; it cannot undo an uncertain save.
    await nativeRequest("desktop.saveCancel", { value: { transfer } }).catch(
      () => {},
    );
    throw error;
  }
}
