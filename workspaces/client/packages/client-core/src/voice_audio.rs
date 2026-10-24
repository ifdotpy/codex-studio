//! macOS audio device layer for realtime voice. A bounded playback queue and an
//! AVAudioEngine capture/playback session sit below the WebRTC/Opus peer in `voice`.
//! Audio stays on native threads; only bounded sample chunks cross into async code.
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

pub const VOICE_RATE: f64 = 48_000.0;

/// Bounded mono 48 kHz playback queue shared between the network task and the
/// audio render thread. `push` runs off the render thread and may block briefly
/// on the same short critical section `pull` uses; `pull` never blocks.
pub struct Playback {
    capacity: usize,
    samples: Mutex<VecDeque<f32>>,
}

impl Playback {
    pub fn new(capacity_samples: usize) -> Arc<Self> {
        Arc::new(Self {
            capacity: capacity_samples,
            samples: Mutex::new(VecDeque::with_capacity(capacity_samples.min(1 << 20))),
        })
    }

    /// Appends samples, dropping the oldest ones first when the queue is full.
    pub fn push(&self, mono: &[f32]) {
        let Ok(mut samples) = self.samples.lock() else {
            return;
        };
        for &sample in mono {
            if samples.len() >= self.capacity {
                samples.pop_front();
            }
            samples.push_back(sample);
        }
    }

    /// Real-time safe: never blocks. Fills `out` from the queue, zero-fills the
    /// rest on underrun or lock contention, and returns the number of real samples.
    pub fn pull(&self, out: &mut [f32]) -> usize {
        let Ok(mut samples) = self.samples.try_lock() else {
            out.fill(0.0);
            return 0;
        };
        let written = out.len().min(samples.len());
        for slot in &mut out[..written] {
            *slot = samples.pop_front().unwrap_or(0.0);
        }
        out[written..].fill(0.0);
        written
    }

    pub fn clear(&self) {
        if let Ok(mut samples) = self.samples.lock() {
            samples.clear();
        }
    }
}

#[cfg(target_os = "macos")]
pub mod mac {
    use super::*;
    use block2::RcBlock;
    use objc2::{
        AnyThread,
        rc::Retained,
        runtime::{AnyObject, Bool, NSObjectProtocol, ProtocolObject},
    };
    use objc2_avf_audio::{
        AVAudioEngine, AVAudioEngineConfigurationChangeNotification, AVAudioFormat,
        AVAudioFrameCount, AVAudioPCMBuffer, AVAudioSourceNode, AVAudioTime,
    };
    use objc2_core_audio_types::{AudioBuffer, AudioBufferList, AudioTimeStamp};
    use objc2_foundation::NSNotificationCenter;
    use std::{
        cell::RefCell,
        collections::HashMap,
        ptr::NonNull,
        sync::atomic::{AtomicBool, Ordering},
    };
    use tokio::sync::mpsc;

    /// `OSStatus` from `objc2-core-audio-types` is crate-private there; the render
    /// block ABI only needs the same `i32` representation.
    type OSStatus = i32;
    const NO_ERROR: OSStatus = 0;

    struct Session {
        engine: Retained<AVAudioEngine>,
        source: Retained<AVAudioSourceNode>,
        observer: Retained<ProtocolObject<dyn NSObjectProtocol>>,
    }
    thread_local! {
        static SESSIONS: RefCell<HashMap<String, Session>> = RefCell::new(HashMap::new());
    }

    fn playback_format() -> Result<Retained<AVAudioFormat>, String> {
        unsafe {
            AVAudioFormat::initStandardFormatWithSampleRate_channels(
                AVAudioFormat::alloc(),
                VOICE_RATE,
                1,
            )
        }
        .ok_or_else(|| "Playback audio format is unavailable.".to_owned())
    }

    /// Builds the AVAudioSourceNode that pulls from `playback` and renders it.
    /// Shared by the live capture session and the offline render check, so the
    /// check exercises the exact render block used in production.
    pub fn make_playback_node(
        playback: &Arc<super::Playback>,
        format: &AVAudioFormat,
    ) -> Retained<AVAudioSourceNode> {
        let playback = playback.clone();
        let block = RcBlock::new(
            move |is_silence: NonNull<Bool>,
                  _timestamp: NonNull<AudioTimeStamp>,
                  frame_count: AVAudioFrameCount,
                  output_data: NonNull<AudioBufferList>|
                  -> OSStatus {
                let frames = frame_count as usize;
                let list: &mut AudioBufferList = unsafe { &mut *output_data.as_ptr() };
                let buffer: &mut AudioBuffer = &mut list.mBuffers[0];
                let data = buffer.mData as *mut f32;
                if data.is_null() || frames == 0 {
                    return NO_ERROR;
                }
                let out = unsafe { std::slice::from_raw_parts_mut(data, frames) };
                let written = playback.pull(out);
                buffer.mDataByteSize = (frames * std::mem::size_of::<f32>()) as u32;
                unsafe { *is_silence.as_ptr() = Bool::new(written == 0) };
                NO_ERROR
            },
        );
        unsafe {
            AVAudioSourceNode::initWithFormat_renderBlock(
                AVAudioSourceNode::alloc(),
                format,
                RcBlock::as_ptr(&block),
            )
        }
    }

    /// Notifies `on_changed` once when the engine's device configuration changes
    /// (for example, the active input device disappearing).
    fn watch_configuration_changes(
        engine: &Retained<AVAudioEngine>,
        on_changed: impl Fn() + Send + Sync + 'static,
    ) -> Retained<ProtocolObject<dyn NSObjectProtocol>> {
        let engine_object: Retained<AnyObject> =
            unsafe { Retained::cast_unchecked(Retained::clone(engine)) };
        let handler =
            RcBlock::new(move |_note: NonNull<objc2_foundation::NSNotification>| on_changed());
        unsafe {
            NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
                Some(AVAudioEngineConfigurationChangeNotification),
                Some(&engine_object),
                None,
                &handler,
            )
        }
    }

    /// Main thread only. Starts one AVAudioEngine with voice processing (echo
    /// cancellation) enabled on the input node, taps the input, converts to
    /// 48 kHz mono f32 with `crate::media::Resampler`, and `try_send`s chunks to
    /// `microphone`. Playback runs through an AVAudioSourceNode on the same
    /// engine's main mixer, so voice processing has the echo reference.
    pub fn start(
        id: String,
        microphone: mpsc::Sender<Vec<f32>>,
        playback: Arc<super::Playback>,
        muted: Arc<AtomicBool>,
        ended: Box<dyn Fn(String) + Send + Sync>,
    ) -> Result<(), String> {
        let engine = unsafe { AVAudioEngine::new() };
        let input = unsafe { engine.inputNode() };
        if let Err(error) = unsafe { input.setVoiceProcessingEnabled_error(true) } {
            return Err(error.localizedDescription().to_string());
        }
        let capture_format = unsafe { input.outputFormatForBus(0) };
        let (rate, channels) = unsafe {
            (
                capture_format.sampleRate(),
                capture_format.channelCount() as usize,
            )
        };
        if rate.is_nan() || rate < 8000.0 || channels == 0 {
            return Err("No microphone input is available.".into());
        }

        let format = playback_format()?;
        let source = make_playback_node(&playback, &format);
        unsafe {
            engine.attachNode(&source);
            let mixer = engine.mainMixerNode();
            engine.connect_to_format(&source, &mixer, Some(&format));
        }

        let ended = Arc::new(ended);
        let failed = Arc::new(AtomicBool::new(false));
        let resampler = Arc::new(Mutex::new(crate::media::Resampler::new(rate, VOICE_RATE)));
        let (tap_resampler, tap_sender, tap_muted, tap_failed, tap_id, tap_ended) = (
            resampler,
            microphone,
            muted,
            failed.clone(),
            id.clone(),
            ended.clone(),
        );
        let end_once = move |id: String,
                             ended: &Arc<Box<dyn Fn(String) + Send + Sync>>,
                             failed: &Arc<AtomicBool>| {
            if !failed.swap(true, Ordering::SeqCst) {
                dispatch2::DispatchQueue::main().exec_async({
                    let ended = ended.clone();
                    move || {
                        stop(&id);
                        ended(id);
                    }
                });
            }
        };
        let block = RcBlock::new(
            move |buffer: NonNull<AVAudioPCMBuffer>, _time: NonNull<AVAudioTime>| {
                let buffer = unsafe { buffer.as_ref() };
                let frames = unsafe { buffer.frameLength() } as usize;
                let data = unsafe { buffer.floatChannelData() };
                if data.is_null() || frames == 0 {
                    return;
                }
                let mut mono = vec![0f32; frames];
                for channel in 0..channels {
                    let samples = unsafe {
                        std::slice::from_raw_parts((*data.add(channel)).as_ptr(), frames)
                    };
                    for (out, value) in mono.iter_mut().zip(samples) {
                        *out += value / channels as f32;
                    }
                }
                if tap_muted.load(Ordering::Relaxed) {
                    mono.iter_mut().for_each(|s| *s = 0.0);
                }
                let Ok(mut resampler) = tap_resampler.lock() else {
                    return;
                };
                let pcm = resampler.process(&mono);
                if pcm.is_empty() {
                    return;
                }
                let samples: Vec<f32> = pcm.iter().map(|&s| s as f32 / 32_768.0).collect();
                if tap_sender.try_send(samples).is_err() {
                    end_once(tap_id.clone(), &tap_ended, &tap_failed);
                }
            },
        );
        unsafe {
            input.installTapOnBus_bufferSize_format_block(
                0,
                4096,
                Some(&capture_format),
                RcBlock::as_ptr(&block),
            );
            engine.prepare();
        }
        if let Err(error) = unsafe { engine.startAndReturnError() } {
            unsafe {
                input.removeTapOnBus(0);
                engine.detachNode(&source);
            }
            return Err(error.localizedDescription().to_string());
        }

        let (watch_id, watch_ended, watch_failed) = (id.clone(), ended, failed);
        let observer = watch_configuration_changes(&engine, move || {
            end_once(watch_id.clone(), &watch_ended, &watch_failed);
        });

        SESSIONS.with_borrow_mut(|sessions| {
            sessions.insert(
                id,
                Session {
                    engine,
                    source,
                    observer,
                },
            )
        });
        Ok(())
    }

    /// Main thread. Removes the tap, stops the engine, and releases the device.
    pub fn stop(id: &str) -> bool {
        let Some(session) = SESSIONS.with_borrow_mut(|sessions| sessions.remove(id)) else {
            return false;
        };
        unsafe {
            NSNotificationCenter::defaultCenter().removeObserver(session.observer.as_ref());
            session.engine.inputNode().removeTapOnBus(0);
            session.engine.stop();
            session.engine.detachNode(&session.source);
        }
        true
    }

    pub fn stop_all() {
        let ids: Vec<String> = SESSIONS.with_borrow(|s| s.keys().cloned().collect());
        for id in ids {
            stop(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn pull_zero_fills_on_underrun_and_reports_real_count() {
        let playback = Playback::new(8);
        playback.push(&[1.0, 2.0, 3.0]);
        let mut out = [9.0f32; 5];
        let guard = playback.samples.lock().expect("queue");
        assert_eq!(playback.pull(&mut out), 0);
        assert_eq!(out, [0.0; 5]);
        drop(guard);
        let written = playback.pull(&mut out);
        assert_eq!(written, 3);
        assert_eq!(out, [1.0, 2.0, 3.0, 0.0, 0.0]);
    }

    #[test]
    fn push_drops_oldest_samples_past_capacity() {
        let playback = Playback::new(4);
        playback.push(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        let mut out = [0.0f32; 4];
        let written = playback.pull(&mut out);
        assert_eq!(written, 4);
        assert_eq!(out, [3.0, 4.0, 5.0, 6.0]);
    }

    #[test]
    fn push_after_pull_still_respects_capacity() {
        let playback = Playback::new(3);
        playback.push(&[1.0, 2.0]);
        let mut out = [0.0f32; 1];
        assert_eq!(playback.pull(&mut out), 1);
        assert_eq!(out, [1.0]);
        playback.push(&[3.0, 4.0, 5.0]);
        let mut out = [0.0f32; 8];
        let written = playback.pull(&mut out);
        assert_eq!(written, 3);
        assert_eq!(&out[..3], &[3.0, 4.0, 5.0]);
    }

    #[test]
    fn clear_empties_the_queue() {
        let playback = Playback::new(8);
        playback.push(&[1.0, 2.0, 3.0]);
        playback.clear();
        let mut out = [9.0f32; 3];
        let written = playback.pull(&mut out);
        assert_eq!(written, 0);
        assert_eq!(out, [0.0, 0.0, 0.0]);
    }

    #[test]
    fn concurrent_push_and_pull_preserve_order_and_drop_no_samples_in_flight() {
        let playback = Playback::new(1 << 16);
        let total = 20_000usize;
        let start = Arc::new(std::sync::Barrier::new(2));
        let producer = {
            let playback = playback.clone();
            let start = start.clone();
            thread::spawn(move || {
                start.wait();
                for chunk in (0..total).collect::<Vec<_>>().chunks(37) {
                    let values: Vec<f32> = chunk.iter().map(|&n| n as f32).collect();
                    playback.push(&values);
                }
            })
        };
        start.wait();
        let mut collected = Vec::with_capacity(total);
        let mut out = [0.0f32; 41];
        loop {
            // Observe completion before pulling so an empty final drain cannot race a writer.
            let finished = producer.is_finished();
            let written = playback.pull(&mut out);
            collected.extend_from_slice(&out[..written]);
            if finished && written == 0 {
                break;
            }
            thread::yield_now();
        }
        producer.join().unwrap();
        assert_eq!(collected.len(), total);
        assert_eq!(collected, (0..total).map(|n| n as f32).collect::<Vec<_>>());
    }
}
