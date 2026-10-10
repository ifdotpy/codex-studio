import workletURL from "./capture-worklet.js?url";
export async function captureAudio(onChunk: (pcm: Int16Array) => void) {
  const stream = await navigator.mediaDevices.getUserMedia({
    audio: { channelCount: 1 },
    video: false,
  });
  let context: AudioContext;
  try {
    context = new AudioContext({ sampleRate: 16000 });
  } catch (error) {
    stream.getTracks().forEach((track) => track.stop());
    throw error;
  }
  try {
    await context.audioWorklet.addModule(workletURL);
    const source = context.createMediaStreamSource(stream);
    const node = new AudioWorkletNode(context, "studio-capture");
    const mute = context.createGain();
    mute.gain.value = 0;
    source.connect(node).connect(mute).connect(context.destination);
    await context.resume();
    let stopped: (() => void) | undefined;
    node.port.onmessage = (event) => {
      if (event.data === "stopped") stopped?.();
      else onChunk(event.data as Int16Array);
    };
    return {
      sampleRate: context.sampleRate,
      stop: async () => {
        const flushed = await Promise.race([
          new Promise<boolean>((resolve) => {
            stopped = () => resolve(true);
            node.port.postMessage("stop");
          }),
          new Promise<boolean>((resolve) =>
            setTimeout(() => resolve(false), 500),
          ),
        ]);
        stream.getTracks().forEach((track) => track.stop());
        source.disconnect();
        node.disconnect();
        await context.close();
        if (!flushed)
          throw Error(
            "The final audio chunk was not confirmed. Earlier saved audio remains available.",
          );
      },
    };
  } catch (error) {
    stream.getTracks().forEach((track) => track.stop());
    await context.close();
    throw error;
  }
}
