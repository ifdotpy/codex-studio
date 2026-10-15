//! Device-free render check for the realtime voice playback path. No microphone,
//! no live audio device, no sound: the engine renders offline into memory
//! buffers using the same production node-construction function as `mac::start`.
//! Live microphone capture and echo cancellation are not verified by this check.
use objc2::{AnyThread, rc::Retained};
use objc2_avf_audio::{AVAudioEngine, AVAudioEngineManualRenderingMode, AVAudioFormat};
use objc2_core_audio_types::{AudioBuffer, AudioBufferList};
use std::{ffi::c_void, ptr::NonNull};
use studio_client_core::voice_audio::{VOICE_RATE, mac};

fn tone(rate: f64, frequency: f64, seconds: f64) -> Vec<f32> {
    (0..(rate * seconds) as usize)
        .map(|n| (0.5 * (2.0 * std::f64::consts::PI * frequency * n as f64 / rate).sin()) as f32)
        .collect()
}

fn rms(samples: &[f32]) -> f64 {
    (samples.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / samples.len() as f64).sqrt()
}

fn render_offline(engine: &Retained<AVAudioEngine>, frames: usize) -> Vec<f32> {
    let block: &block2::DynBlock<_> = unsafe { &*engine.manualRenderingBlock() };
    let mut output = vec![0f32; frames];
    let mut list = AudioBufferList {
        mNumberBuffers: 1,
        mBuffers: [AudioBuffer {
            mNumberChannels: 1,
            mDataByteSize: (frames * std::mem::size_of::<f32>()) as u32,
            mData: output.as_mut_ptr() as *mut c_void,
        }],
    };
    let mut error = 0i32;
    let status = block.call((
        frames as u32,
        NonNull::new(&mut list as *mut _).unwrap(),
        &mut error as *mut _,
    ));
    assert_eq!(
        status.0, 0,
        "manual rendering status {status:?}, error {error}"
    );
    output
}

fn check() -> Result<(), String> {
    let playback = studio_client_core::voice_audio::Playback::new(VOICE_RATE as usize * 2);
    playback.push(&tone(VOICE_RATE, 1000.0, 1.0));

    let format = unsafe {
        AVAudioFormat::initStandardFormatWithSampleRate_channels(
            AVAudioFormat::alloc(),
            VOICE_RATE,
            1,
        )
    }
    .ok_or("Playback audio format is unavailable.")?;
    let engine = unsafe { AVAudioEngine::new() };
    // Reuses the exact node the live session attaches: no voice processing,
    // no input node, no hardware device involved.
    let node = mac::make_playback_node(&playback, &format);
    unsafe {
        engine.attachNode(&node);
        let mixer = engine.mainMixerNode();
        engine.connect_to_format(&node, &mixer, Some(&format));
        engine
            .enableManualRenderingMode_format_maximumFrameCount_error(
                AVAudioEngineManualRenderingMode::Offline,
                &format,
                4096,
            )
            .map_err(|e| e.localizedDescription().to_string())?;
        engine
            .startAndReturnError()
            .map_err(|e| e.localizedDescription().to_string())?;
    }

    let mut rendered = Vec::new();
    for _ in 0..50 {
        rendered.extend(render_offline(&engine, 480));
    }
    unsafe { engine.stop() };

    if rendered.len() != 24_000 {
        return Err(format!("Unexpected render length: {}", rendered.len()));
    }
    let middle = &rendered[4_000..20_000];
    let level = rms(middle);
    if !(0.30..=0.40).contains(&level) {
        return Err(format!("Unexpected RMS: {level}"));
    }
    let crossings = middle
        .windows(2)
        .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
        .count();
    // A 1 kHz tone over 16,000 samples at 48 kHz is ~333 cycles, ~666 zero crossings.
    if !(600..=730).contains(&crossings) {
        return Err(format!("Unexpected zero crossings: {crossings}"));
    }
    println!(
        "Voice playback render PASS: {} frames offline-rendered from the production node, RMS {level:.3}, {crossings} zero crossings. \
         Live microphone capture and echo cancellation are not verified by this check.",
        rendered.len()
    );
    Ok(())
}

fn main() {
    if let Err(error) = check() {
        eprintln!("Voice playback render FAILED: {error}");
        std::process::exit(1);
    }
}
