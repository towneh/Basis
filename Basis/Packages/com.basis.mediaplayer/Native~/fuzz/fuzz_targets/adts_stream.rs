//! Fuzz the ADTS demuxer: sync hunting, header-length arithmetic and the
//! ASC reconstruction must hold up under arbitrary bytes — typed errors
//! only, no panics, no unbounded loops.

#![no_main]

use libfuzzer_sys::fuzz_target;
use media_clock::Generation;
use media_demux::{AdtsDemuxer, DemuxLimits, Demuxer, MemSource, StreamEvent};

fuzz_target!(|data: &[u8]| {
    let limits = DemuxLimits::default();
    let Ok(mut demux) =
        AdtsDemuxer::open(Box::new(MemSource(data.to_vec())), limits, Generation(1))
    else {
        return;
    };
    for _ in 0..8192 {
        match demux.next_event() {
            Ok(StreamEvent::Eos(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }
    let _ = demux.take_notes();
});
