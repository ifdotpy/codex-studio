//! Replay a recorded identity SSE body with one Brotli dictionary and a flush per event.
//! cargo run --release -p studio-runtime-http --example sync-brotli-replay -- <file> <quality> <lgwin>

use brotli::enc::combined_alloc::BrotliAlloc;
use brotli::enc::writer::{CompressorWriterCustomAlloc, StandardAlloc};
use brotli::{Allocator, SliceWrapper};
use std::cell::RefCell;
use std::io::{Read, Write};
use std::rc::Rc;

#[derive(Default)]
struct Memory {
    live: usize,
    peak: usize,
    allocations: usize,
}

#[derive(Clone)]
struct MeasuredAlloc(Rc<RefCell<Memory>>);

impl<T: Clone + Default> Allocator<T> for MeasuredAlloc {
    type AllocatedMemory = <StandardAlloc as Allocator<T>>::AllocatedMemory;

    fn alloc_cell(&mut self, len: usize) -> Self::AllocatedMemory {
        let mut memory = self.0.borrow_mut();
        memory.live += len * std::mem::size_of::<T>();
        memory.peak = memory.peak.max(memory.live);
        memory.allocations += 1;
        StandardAlloc::default().alloc_cell(len)
    }

    fn free_cell(&mut self, data: Self::AllocatedMemory) {
        self.0.borrow_mut().live -= std::mem::size_of_val(data.slice());
        StandardAlloc::default().free_cell(data);
    }
}

impl BrotliAlloc for MeasuredAlloc {}

// Linux exposes the main thread's scheduled CPU nanoseconds. The current-thread
// runtime keeps encoding on that thread; asynchronous reads use its explicit path.
// Other hosts still report wall time and the encoder's measured heap usage.
async fn thread_cpu_ns() -> Option<u64> {
    tokio::fs::read_to_string("/proc/self/schedstat")
        .await
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: sync-brotli-replay <identity.sse> <quality> <lgwin>".into());
    }
    let input = tokio::fs::read(&args[1]).await?;
    let quality: u32 = args[2].parse()?;
    let lgwin: u32 = args[3].parse()?;
    let memory = Rc::new(RefCell::new(Memory::default()));
    let mut allocator = MeasuredAlloc(Rc::clone(&memory));
    let buffer = allocator.alloc_cell(4096);
    let cpu_start = thread_cpu_ns().await;
    let started = std::time::Instant::now();
    let mut encoder =
        CompressorWriterCustomAlloc::new(Vec::new(), buffer, allocator, quality, lgwin);
    let mut frames = 0;
    let mut max_flush_us = 0;
    let mut compressed = Vec::new();
    let mut offset = 0;
    for end in input
        .windows(2)
        .enumerate()
        .filter_map(|(i, bytes)| (bytes == b"\n\n").then_some(i + 2))
    {
        let frame_start = std::time::Instant::now();
        encoder.write_all(&input[offset..end])?;
        encoder.flush()?;
        max_flush_us = max_flush_us.max(frame_start.elapsed().as_micros());
        compressed.extend_from_slice(encoder.get_ref());
        encoder.get_mut().clear();
        offset = end;
        frames += 1;
    }
    if offset != input.len() {
        return Err("recording ends with an incomplete SSE event".into());
    }
    compressed.extend_from_slice(&encoder.into_inner());
    let wall_us = started.elapsed().as_micros();
    let cpu_us = cpu_start
        .zip(thread_cpu_ns().await)
        .map(|(start, end)| (end - start) / 1000);
    let memory = memory.borrow();
    let mut decoded = Vec::new();
    brotli::Decompressor::new(compressed.as_slice(), 4096).read_to_end(&mut decoded)?;
    if decoded != input {
        return Err("decoded replay differs from recording".into());
    }
    println!(
        "{}",
        serde_json::json!({
            "quality":quality,"lgwin":lgwin,"events":frames,"plainBytes":input.len(),
            "compressedBytes":compressed.len(),"wallUs":wall_us,"cpuUs":cpu_us,
            "maxEventWallUs":max_flush_us,"peakEncoderHeapBytes":memory.peak,
            "encoderAllocations":memory.allocations,
        })
    );
    Ok(())
}
