//! Fuzz the raw FLAC demuxer: arbitrary bytes must produce typed errors
//! or a walkable event stream, never a panic, out-of-bounds read or
//! unbounded loop. Covers the metadata walk, frame-header validation
//! (CRC-8) and the frame-boundary scan.

#![no_main]

use libfuzzer_sys::fuzz_target;
use media_clock::Generation;
use media_demux::{DemuxLimits, Demuxer, FlacDemuxer, MemSource, StreamEvent};

fuzz_target!(|data: &[u8]| {
    let limits = DemuxLimits {
        max_metadata_bytes: 4 * 1024 * 1024,
        max_au_bytes: 4 * 1024 * 1024,
    };
    let Ok(mut demux) =
        FlacDemuxer::open(Box::new(MemSource(data.to_vec())), limits, Generation(1))
    else {
        return;
    };
    for _ in 0..8192 {
        match demux.next_event() {
            Ok(StreamEvent::Eos(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }
});
