//! Fuzz the streaming MP4 demuxer end to end: arbitrary bytes must produce
//! typed errors or a walkable event stream, never a panic escaping the
//! demuxer or an out-of-bounds read. (The open path deliberately contains
//! re_mp4's own panic paths as typed errors; everything after open — sample
//! walking, Annex-B conversion, seek — runs unfenced and is what this
//! target actually exercises.)

#![no_main]

use libfuzzer_sys::fuzz_target;
use media_clock::{Generation, MediaTime};
use media_demux::{DemuxLimits, Demuxer, MemSource, Mp4Demuxer, StreamEvent};

fuzz_target!(|data: &[u8]| {
    let limits = DemuxLimits {
        // Small caps keep iterations fast and exercise the cap paths.
        max_metadata_bytes: 4 * 1024 * 1024,
        max_au_bytes: 4 * 1024 * 1024,
    };
    let Ok(mut demux) = Mp4Demuxer::open(
        Box::new(MemSource(data.to_vec())),
        limits,
        Generation(1),
    ) else {
        return;
    };
    let _ = demux.duration();
    let _ = demux.take_notes();
    for _ in 0..4096 {
        match demux.next_event() {
            Ok(StreamEvent::Eos(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }
    let _ = demux.seek(MediaTime::from_secs(1), Generation(2));
    for _ in 0..64 {
        match demux.next_event() {
            Ok(StreamEvent::Eos(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }
});
