//! Sender reports that arrive after frames are already flowing move the
//! video onto the audio's timeline. Audio keeps its own: downstream it is
//! the clock master and never gives way.

use media_rtsp::{MAX_STREAMS, Realign, StreamAlign, realign_video};

const VIDEO: usize = 0;
const AUDIO: usize = 1;

/// NTP 32.32 for a whole number of microseconds past an arbitrary epoch.
fn ntp(us: u64) -> u64 {
    const EPOCH: u64 = 3_999_516_038 << 32;
    EPOCH + ((us / 1_000_000) << 32) + (((us % 1_000_000) << 32).div_ceil(1_000_000))
}

fn aligned(video_zero_us: u64, audio_zero_us: u64) -> [StreamAlign; MAX_STREAMS] {
    let mut align = [StreamAlign::default(); MAX_STREAMS];
    align[VIDEO].ntp_at_zero = Some(ntp(video_zero_us));
    align[AUDIO].ntp_at_zero = Some(ntp(audio_zero_us));
    align
}

#[test]
fn late_reports_move_the_video_and_leave_the_audio() {
    // Video's zero is 156.25 ms after audio's (1/64 s steps are exact in
    // NTP's 32.32): its frames belong that much later.
    let mut align = aligned(10_156_250, 10_000_000);
    assert_eq!(
        realign_video(&mut align, VIDEO, AUDIO),
        Realign::Moved(156_250)
    );
    assert_eq!(align[VIDEO].offset_us, 156_250);
    assert_eq!(align[AUDIO].offset_us, 0);

    // And 78.125 ms before it: the video moves back.
    let mut align = aligned(9_921_875, 10_000_000);
    assert_eq!(
        realign_video(&mut align, VIDEO, AUDIO),
        Realign::Moved(-78_125)
    );
    assert_eq!(align[VIDEO].offset_us, -78_125);
    assert_eq!(align[AUDIO].offset_us, 0);
}

#[test]
fn the_step_is_measured_from_the_offsets_already_applied() {
    let mut align = aligned(10_156_250, 10_000_000);
    align[VIDEO].offset_us = 40_000;
    align[AUDIO].offset_us = 25_000;
    assert_eq!(
        realign_video(&mut align, VIDEO, AUDIO),
        Realign::Moved(141_250)
    );
    assert_eq!(align[VIDEO].offset_us, 181_250);
    assert_eq!(align[AUDIO].offset_us, 25_000, "audio never gives way");

    // Already where the reports put it: nothing moves.
    assert_eq!(realign_video(&mut align, VIDEO, AUDIO), Realign::Unchanged);
    assert_eq!(align[VIDEO].offset_us, 181_250);
}

#[test]
fn a_step_past_three_seconds_is_refused() {
    let mut align = aligned(13_000_000, 10_000_000);
    assert_eq!(
        realign_video(&mut align, VIDEO, AUDIO),
        Realign::Moved(3_000_000)
    );

    let mut align = aligned(13_000_001, 10_000_000);
    assert_eq!(
        realign_video(&mut align, VIDEO, AUDIO),
        Realign::Refused(3_000_001)
    );
    assert_eq!(
        align[VIDEO].offset_us, 0,
        "a refused step leaves the start's alignment"
    );

    let mut align = aligned(10_000_000, 13_000_001);
    assert_eq!(
        realign_video(&mut align, VIDEO, AUDIO),
        Realign::Refused(-3_000_001)
    );
}

#[test]
fn a_stream_without_a_report_leaves_everything_alone() {
    let mut align = aligned(10_156_250, 10_000_000);
    align[AUDIO].ntp_at_zero = None;
    assert_eq!(realign_video(&mut align, VIDEO, AUDIO), Realign::Unchanged);
    assert_eq!(align[VIDEO].offset_us, 0);
}

mod start_without_reports {
    use std::collections::VecDeque;

    use media_clock::Generation;
    use media_demux::StreamEvent;
    use media_rtsp::{MAX_STREAMS, PendingFrame, StreamAlign, flush_aligned};
    use tokio::sync::mpsc;

    use super::{AUDIO, VIDEO, ntp};

    fn frame(stream_id: usize, elapsed_us: i64) -> PendingFrame {
        PendingFrame {
            stream_id,
            data: vec![0],
            elapsed_us,
            key: true,
        }
    }

    /// Flush these frames as the aligner does when its wait ends, and
    /// return each emitted frame's (stream, pts in µs).
    fn flush(frames: Vec<PendingFrame>, align: &mut [StreamAlign; MAX_STREAMS]) -> Vec<(u32, i64)> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (tx, mut rx) = mpsc::channel(64);
            let mut buffered: VecDeque<PendingFrame> = frames.into();
            flush_aligned(
                &mut buffered,
                align,
                &[VIDEO, AUDIO],
                Generation::default(),
                &tx,
            )
            .await
            .expect("flush");
            drop(tx);
            let mut out = Vec::new();
            while let Some(Ok(StreamEvent::Au(au))) = rx.recv().await {
                out.push((au.track.0, au.pts.as_micros()));
            }
            out
        })
    }

    /// Start times that put the first frames further apart than a live
    /// join delivers them are not believed: each stream counts from its
    /// first frame.
    #[test]
    fn start_times_far_from_the_frames_give_way_to_the_first_frames() {
        let mut align = [StreamAlign::default(); MAX_STREAMS];
        let out = flush(
            vec![
                frame(AUDIO, 400_000),
                frame(VIDEO, 0),
                frame(VIDEO, 40_000),
                frame(AUDIO, 421_333),
            ],
            &mut align,
        );
        assert_eq!(out, [(1, 0), (0, 0), (0, 40_000), (1, 21_333)]);
    }

    /// Within 250 ms they are kept, and the frames flow on them unchanged.
    #[test]
    fn start_times_within_250_ms_are_kept() {
        let mut align = [StreamAlign::default(); MAX_STREAMS];
        let out = flush(vec![frame(AUDIO, 250_000), frame(VIDEO, 0)], &mut align);
        assert_eq!(out, [(1, 250_000), (0, 0)]);

        let mut align = [StreamAlign::default(); MAX_STREAMS];
        let out = flush(vec![frame(AUDIO, 250_001), frame(VIDEO, 0)], &mut align);
        assert_eq!(out, [(1, 0), (0, 0)]);
    }

    /// With every stream's report in, the reports decide, whatever the
    /// frames say.
    #[test]
    fn reports_outrank_the_check() {
        let mut align = [StreamAlign::default(); MAX_STREAMS];
        align[VIDEO].ntp_at_zero = Some(ntp(10_000_000));
        align[AUDIO].ntp_at_zero = Some(ntp(10_000_000));
        let out = flush(vec![frame(AUDIO, 400_000), frame(VIDEO, 0)], &mut align);
        assert_eq!(out, [(1, 400_000), (0, 0)]);
    }
}
