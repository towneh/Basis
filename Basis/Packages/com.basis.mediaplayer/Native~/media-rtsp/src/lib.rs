#![forbid(unsafe_code)]

//! RTSP session as a [`Demuxer`] (§6.6, §5.5): `retina` owns the RTSP
//! state machine. `rtsp://` negotiates UDP first — media-rtp's reorder/
//! RTCP-RR layer under retina's signalling (`udp` module), because
//! retina's own UDP path has no reorder buffer and sends no receiver
//! reports, which real servers kill sessions over — and falls back to
//! TCP-interleaved when UDP cannot be set up or no datagrams flow.
//! `rtspt://` pins TCP-interleaved; the Bank downstream is the jitter
//! answer either way.
//!
//! A/V alignment: RTP timestamps are per-stream, so cross-stream offsets
//! come from RTCP sender reports (NTP ↔ RTP mappings). Frames buffer
//! briefly at start until every stream has a sender report (or a bounded
//! wait expires, falling back to join-skew alignment), then flow with
//! aligned timestamps — the Bank's startup hold is filling during that
//! window anyway, so the join pays nothing extra.

use std::collections::VecDeque;
use std::time::Duration;

use futures::StreamExt;
use media_clock::{Generation, MediaTime};
use media_demux::{
    Au, AudioCodec, DemuxError, Demuxer, EosReason, Format, StreamEvent, TrackId, VideoCodec,
};
use media_rtp::{ntp_at_zero, units_to_us};
use retina::client::{PlayOptions, SessionOptions, SetupOptions, Transport};
use retina::codec::{CodecItem, FrameFormat, ParameterSetInsertion, ParametersRef, aac, h26x};
use tokio::sync::mpsc;

mod udp;

pub use udp::UdpPeerAllowed;

/// Channel depth between the session task and the pulling demux thread —
/// also the cap on frames buffered while waiting for sender reports.
/// Public with the shared emit path below: the WHEP lane runs the same
/// session-task → demux-thread shape.
pub const CHANNEL_DEPTH: usize = 512;
/// How long the aligner waits for sender reports before falling back to
/// join-skew alignment.
pub const ALIGN_WAIT: Duration = Duration::from_secs(2);
/// No frames for this long is a dead session (the transport-loss class;
/// the engine's reconnect path takes it from there).
const FEED_STALL: Duration = Duration::from_secs(10);
/// How long a played UDP session may stay silent (no RTP or RTCP on any
/// socket) before the open falls back to TCP-interleaved — the
/// firewall/NAT-blackhole case, invisible at SETUP time.
const UDP_PROBE: Duration = Duration::from_secs(5);

/// The engine's teardown probe: sampled between blocking receives so a
/// closing session never waits out the stall timeout.
pub type CancelProbe = Box<dyn Fn() -> bool + Send>;

/// Which transport the open negotiated (§10: transport choices are
/// diagnosable, never silent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtspTransport {
    Udp,
    TcpInterleaved,
}

impl std::fmt::Display for RtspTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Udp => write!(f, "udp"),
            Self::TcpInterleaved => write!(f, "tcp-interleaved"),
        }
    }
}

pub struct RtspDemuxer {
    rx: mpsc::Receiver<Result<StreamEvent, String>>,
    runtime: tokio::runtime::Handle,
    cancelled: CancelProbe,
    video_track: Option<TrackId>,
    audio_track: Option<TrackId>,
    task: tokio::task::JoinHandle<()>,
    transport: RtspTransport,
    fallback: Option<String>,
}

impl RtspDemuxer {
    /// Open and start pulling. `runtime` hosts the async session (the
    /// shared bm-io runtime); `cancelled` is polled during blocking
    /// waits. `rtsp://` negotiates UDP first and falls back to
    /// TCP-interleaved; `rtspt://` pins TCP-interleaved. `udp_peer_allowed`
    /// vets the UDP peer address from the SETUP response before any
    /// packet is sent to it (§9.3).
    pub fn open(
        url: &str,
        generation: Generation,
        runtime: tokio::runtime::Handle,
        cancelled: CancelProbe,
        udp_peer_allowed: UdpPeerAllowed,
    ) -> Result<Self, DemuxError> {
        let want_udp = url.starts_with("rtsp://");
        let parsed = url::Url::parse(&url.replacen("rtspt://", "rtsp://", 1))
            .map_err(|e| DemuxError::Parse(format!("rtsp url: {e}")))?;

        let mut fallback = None;
        if want_udp {
            match runtime.block_on(udp::setup_udp_session(parsed.clone(), udp_peer_allowed)) {
                Ok(ready) => {
                    let (tx, rx) = mpsc::channel(CHANNEL_DEPTH);
                    let (first_tx, first_rx) = tokio::sync::oneshot::channel();
                    let task = runtime.spawn(async move {
                        let result =
                            udp::run_udp_session(ready, generation, &tx, Some(first_tx)).await;
                        let _ = tx
                            .send(match result {
                                Ok(()) => Ok(StreamEvent::Eos(EosReason::SourceLost)),
                                Err(e) => Err(e),
                            })
                            .await;
                    });
                    // A set-up session that never delivers a datagram is
                    // the UDP-blackhole case; only arrival proves the
                    // path works.
                    match runtime
                        .block_on(async { tokio::time::timeout(UDP_PROBE, first_rx).await })
                    {
                        Ok(Ok(())) => {
                            return Ok(Self {
                                rx,
                                runtime,
                                cancelled,
                                video_track: None,
                                audio_track: None,
                                task,
                                transport: RtspTransport::Udp,
                                fallback: None,
                            });
                        }
                        _ => {
                            task.abort();
                            fallback = Some(format!("no UDP datagrams within {UDP_PROBE:?}"));
                        }
                    }
                }
                Err(detail) => fallback = Some(format!("UDP setup failed: {detail}")),
            }
        }

        // Describe/setup/play happen synchronously so an unreachable or
        // 404 path fails the open itself — the engine's reconnect budget
        // counts it instead of seeing a session that dies on first pull.
        let ready = runtime
            .block_on(setup_session(parsed))
            .map_err(|detail| DemuxError::Source(detail.into()))?;
        let (tx, rx) = mpsc::channel(CHANNEL_DEPTH);
        let task = runtime.spawn(async move {
            let result = run_session(ready, generation, &tx).await;
            let _ = tx
                .send(match result {
                    Ok(()) => Ok(StreamEvent::Eos(EosReason::SourceLost)),
                    Err(e) => Err(e),
                })
                .await;
        });
        Ok(Self {
            rx,
            runtime,
            cancelled,
            video_track: None,
            audio_track: None,
            task,
            transport: RtspTransport::TcpInterleaved,
            fallback,
        })
    }

    pub fn transport(&self) -> RtspTransport {
        self.transport
    }

    /// Why a `rtsp://` open ended up on TCP-interleaved, when it did.
    pub fn fallback_reason(&self) -> Option<&str> {
        self.fallback.as_deref()
    }
}

impl Drop for RtspDemuxer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Demuxer for RtspDemuxer {
    fn next_event(&mut self) -> Result<StreamEvent, DemuxError> {
        let stall_deadline = std::time::Instant::now() + FEED_STALL;
        loop {
            if (self.cancelled)() {
                return Ok(StreamEvent::Eos(EosReason::SourceLost));
            }
            let received = self.runtime.block_on(async {
                tokio::time::timeout(Duration::from_millis(100), self.rx.recv()).await
            });
            match received {
                Ok(Some(Ok(event))) => {
                    if let StreamEvent::Format(track, format) = &event {
                        match format {
                            Format::Video { .. } => self.video_track = Some(*track),
                            Format::Audio { .. } => self.audio_track = Some(*track),
                        }
                    }
                    return Ok(event);
                }
                Ok(Some(Err(detail))) => return Err(DemuxError::Source(detail.into())),
                Ok(None) => return Err(DemuxError::Source("rtsp session task ended".into())),
                Err(_) => {
                    if std::time::Instant::now() >= stall_deadline {
                        return Err(DemuxError::Source("rtsp feed stalled".into()));
                    }
                }
            }
        }
    }

    fn seek(
        &mut self,
        _target: MediaTime,
        _generation: Generation,
    ) -> Result<MediaTime, DemuxError> {
        Err(DemuxError::Unsupported("seek on an RTSP session"))
    }

    fn duration(&self) -> Option<MediaTime> {
        None
    }

    fn video_track(&self) -> Option<TrackId> {
        self.video_track
    }

    fn audio_track(&self) -> Option<TrackId> {
        self.audio_track
    }
}

/// One depacketised frame awaiting emission. Part of the shared
/// RTP-session emit path: the RTSP drivers and the WHEP lane all feed
/// frames through [`emit`]/[`flush_aligned`] so A/V alignment and the
/// `StreamEvent` conversion exist exactly once.
pub struct PendingFrame {
    pub stream_id: usize,
    pub data: Vec<u8>,
    pub elapsed_us: i64,
    pub key: bool,
}

/// Per-stream alignment: NTP time (32.32) at stream elapsed 0, learned
/// from a sender report, and the microsecond offset derived from it.
#[derive(Default, Clone, Copy)]
pub struct StreamAlign {
    pub ntp_at_zero: Option<u64>,
    pub offset_us: i64,
}

pub const MAX_STREAMS: usize = 8;

/// First H.264 video stream and first AAC audio stream; breadth grows
/// with the §6.7 decode table.
pub(crate) fn select_streams(streams: &[retina::client::Stream]) -> (Option<usize>, Option<usize>) {
    let mut video_index = None;
    let mut audio_index = None;
    for (index, stream) in streams.iter().enumerate() {
        match (stream.media(), stream.encoding_name()) {
            ("video", "h264") if video_index.is_none() && index < MAX_STREAMS => {
                video_index = Some(index);
            }
            ("audio", "mpeg4-generic") if audio_index.is_none() && index < MAX_STREAMS => {
                audio_index = Some(index);
            }
            _ => {}
        }
    }
    (video_index, audio_index)
}

/// Annex-B with parameter sets on every keyframe, raw AAC with the
/// AudioSpecificConfig out of band — the decode adapters' contracts.
pub fn frame_format() -> FrameFormat {
    FrameFormat {
        h26x_framing: h26x::Framing::AnnexB,
        parameter_set_insertion: ParameterSetInsertion::EachKeyFrame,
        aac_framing: aac::Framing::Raw,
    }
}

pub fn ntp_delta_us(ntp: u64) -> i64 {
    // 32.32 fixed → microseconds. Relative use only; the epoch cancels.
    let secs = (ntp >> 32) as i64;
    let frac = (ntp & 0xFFFF_FFFF) as i64;
    secs.saturating_mul(1_000_000) + ((frac * 1_000_000) >> 32)
}

/// A session set up and playing, ready for the pull loop.
struct ReadySession {
    session: retina::client::Session<retina::client::Playing>,
    video_index: Option<usize>,
    audio_index: Option<usize>,
}

async fn setup_session(url: url::Url) -> Result<ReadySession, String> {
    let options = SessionOptions::default().user_agent("basis-media".into());
    let mut session = retina::client::Session::describe(url, options)
        .await
        .map_err(|e| format!("rtsp describe: {e}"))?;

    let (video_index, audio_index) = select_streams(session.streams());
    if video_index.is_none() && audio_index.is_none() {
        return Err("no h264 video or aac audio stream in the SDP".into());
    }

    for index in [video_index, audio_index].into_iter().flatten() {
        session
            .setup(
                index,
                SetupOptions::default()
                    .transport(Transport::Tcp(Default::default()))
                    .frame_format(frame_format()),
            )
            .await
            .map_err(|e| format!("rtsp setup: {e}"))?;
    }

    // Permissive: servers legally omit `rtptime` from RTP-Info for
    // streams that have not seen data yet (mediamtx does on quiet paths).
    let session = session
        .play(
            PlayOptions::default()
                .initial_timestamp(retina::client::InitialTimestampPolicy::Permissive),
        )
        .await
        .map_err(|e| format!("rtsp play: {e}"))?;
    Ok(ReadySession {
        session,
        video_index,
        audio_index,
    })
}

async fn run_session(
    ready: ReadySession,
    generation: Generation,
    tx: &mpsc::Sender<Result<StreamEvent, String>>,
) -> Result<(), String> {
    let ReadySession {
        session,
        video_index,
        audio_index,
    } = ready;
    let needed: Vec<usize> = [video_index, audio_index].into_iter().flatten().collect();

    // Announce formats known from the SDP up front.
    let mut announced = [false; MAX_STREAMS];
    for index in [video_index, audio_index].into_iter().flatten() {
        if let Some(parameters) = session.streams()[index].parameters()
            && let Some(format) = format_from(&parameters)
        {
            send_event(tx, StreamEvent::Format(TrackId(index as u32), format)).await?;
            announced[index] = true;
        }
    }

    let mut demuxed = session.demuxed().map_err(|e| format!("rtsp demux: {e}"))?;

    // Alignment: buffer until every set-up stream has a sender report or
    // the wait expires, then flush with per-stream offsets applied.
    let mut align = [StreamAlign::default(); MAX_STREAMS];
    let mut buffered: VecDeque<PendingFrame> = VecDeque::new();
    let mut aligning = true;
    let align_deadline = tokio::time::Instant::now() + ALIGN_WAIT;

    loop {
        let item = if aligning {
            match tokio::time::timeout_at(align_deadline, demuxed.next()).await {
                Ok(item) => item,
                Err(_) => {
                    flush_aligned(&mut buffered, &mut align, &needed, generation, tx).await?;
                    aligning = false;
                    continue;
                }
            }
        } else {
            demuxed.next().await
        };
        let Some(item) = item else {
            return Ok(());
        };
        let item = item.map_err(|e| format!("rtsp stream: {e}"))?;

        match item {
            CodecItem::VideoFrame(frame) => {
                let stream_id = frame.stream_id();
                if (frame.has_new_parameters()
                    || !announced.get(stream_id).copied().unwrap_or(true))
                    && let Some(parameters) = demuxed.streams()[stream_id].parameters()
                    && let Some(format) = format_from(&parameters)
                {
                    send_event(tx, StreamEvent::Format(TrackId(stream_id as u32), format)).await?;
                    if let Some(flag) = announced.get_mut(stream_id) {
                        *flag = true;
                    }
                }
                let timestamp = frame.timestamp();
                let elapsed_us = units_to_us(timestamp.elapsed(), timestamp.clock_rate());
                let pending = PendingFrame {
                    stream_id,
                    elapsed_us,
                    key: frame.is_random_access_point(),
                    data: frame.into_data(),
                };
                emit(pending, aligning, &mut buffered, &align, generation, tx).await?;
            }
            CodecItem::AudioFrame(frame) => {
                let stream_id = frame.stream_id();
                if !announced.get(stream_id).copied().unwrap_or(true)
                    && let Some(parameters) = demuxed.streams()[stream_id].parameters()
                    && let Some(format) = format_from(&parameters)
                {
                    send_event(tx, StreamEvent::Format(TrackId(stream_id as u32), format)).await?;
                    if let Some(flag) = announced.get_mut(stream_id) {
                        *flag = true;
                    }
                }
                let timestamp = frame.timestamp();
                let elapsed_us = units_to_us(timestamp.elapsed(), timestamp.clock_rate());
                let pending = PendingFrame {
                    stream_id,
                    elapsed_us,
                    key: true,
                    data: frame.data().to_vec(),
                };
                emit(pending, aligning, &mut buffered, &align, generation, tx).await?;
            }
            CodecItem::Rtcp(rtcp) => {
                let stream_id = rtcp.stream_id();
                if stream_id < MAX_STREAMS
                    && align[stream_id].ntp_at_zero.is_none()
                    && let Some(rtp_timestamp) = rtcp.rtp_timestamp()
                {
                    let elapsed_us =
                        units_to_us(rtp_timestamp.elapsed(), rtp_timestamp.clock_rate());
                    // The first report in the compound packet, not the last
                    // one: a sender is free to carry several, and letting
                    // each overwrite the one before makes the anchor depend
                    // on how the peer packed them.
                    for packet in rtcp.pkts() {
                        if let Ok(Some(sr)) = packet.as_sender_report() {
                            align[stream_id].ntp_at_zero =
                                Some(ntp_at_zero(sr.ntp_timestamp().0, elapsed_us));
                            break;
                        }
                    }
                }
                if aligning && needed.iter().all(|&i| align[i].ntp_at_zero.is_some()) {
                    flush_aligned(&mut buffered, &mut align, &needed, generation, tx).await?;
                    aligning = false;
                }
            }
            _ => {}
        }

        if aligning && buffered.len() >= CHANNEL_DEPTH {
            flush_aligned(&mut buffered, &mut align, &needed, generation, tx).await?;
            aligning = false;
        }
    }
}

/// Send one event to the pulling demux thread; `Err` when it hung up.
pub async fn send_event(
    tx: &mpsc::Sender<Result<StreamEvent, String>>,
    event: StreamEvent,
) -> Result<(), String> {
    tx.send(Ok(event))
        .await
        .map_err(|_| "engine hung up".into())
}

/// Compute per-stream offsets from the collected sender reports and flush
/// the buffered frames in arrival order.
pub async fn flush_aligned(
    buffered: &mut VecDeque<PendingFrame>,
    align: &mut [StreamAlign; MAX_STREAMS],
    needed: &[usize],
    generation: Generation,
    tx: &mpsc::Sender<Result<StreamEvent, String>>,
) -> Result<(), String> {
    let ntp_zeroes: Vec<(usize, u64)> = needed
        .iter()
        .filter_map(|&index| align[index].ntp_at_zero.map(|ntp| (index, ntp)))
        .collect();
    if ntp_zeroes.len() == needed.len() && !ntp_zeroes.is_empty() {
        // Common origin: the earliest stream start carries offset 0,
        // later streams positive offsets. Without full reports, offsets
        // stay zero — join-skew alignment.
        let min_ntp = ntp_zeroes
            .iter()
            .map(|&(_, ntp)| ntp)
            .min()
            .expect("non-empty");
        for &(index, ntp) in &ntp_zeroes {
            align[index].offset_us = ntp_delta_us(ntp.wrapping_sub(min_ntp));
        }
    }
    for frame in std::mem::take(buffered) {
        emit_aligned(frame, align, generation, tx).await?;
    }
    Ok(())
}

/// Route one frame: buffered while the aligner is collecting sender
/// reports, emitted with per-stream offsets applied once it settles.
pub async fn emit(
    frame: PendingFrame,
    aligning: bool,
    buffered: &mut VecDeque<PendingFrame>,
    align: &[StreamAlign; MAX_STREAMS],
    generation: Generation,
    tx: &mpsc::Sender<Result<StreamEvent, String>>,
) -> Result<(), String> {
    if aligning {
        buffered.push_back(frame);
        return Ok(());
    }
    emit_aligned(frame, align, generation, tx).await
}

async fn emit_aligned(
    frame: PendingFrame,
    align: &[StreamAlign; MAX_STREAMS],
    generation: Generation,
    tx: &mpsc::Sender<Result<StreamEvent, String>>,
) -> Result<(), String> {
    let offset = align.get(frame.stream_id).map(|a| a.offset_us).unwrap_or(0);
    // RTP carries presentation time only; arrival order is decode order,
    // so dts = pts (live encoders on these lanes do not reorder).
    let pts = MediaTime::from_micros(frame.elapsed_us.saturating_add(offset));
    let au = Au {
        track: TrackId(frame.stream_id as u32),
        data: frame.data,
        pts,
        dts: pts,
        key: frame.key,
        generation,
    };
    tx.send(Ok(StreamEvent::Au(au)))
        .await
        .map_err(|_| "engine hung up".into())
}

/// [`Format`] announce from retina stream parameters — the depacketizer
/// side of the shared emit path.
pub fn format_from(parameters: &ParametersRef<'_>) -> Option<Format> {
    match parameters {
        ParametersRef::Video(video) => {
            let (width, height) = video.pixel_dimensions();
            Some(Format::Video {
                codec: VideoCodec::H264,
                coded_width: width,
                coded_height: height,
                display_width: width,
                display_height: height,
                codec_private: Vec::new(),
            })
        }
        ParametersRef::Audio(audio) => Some(Format::Audio {
            codec: AudioCodec::Aac,
            sample_rate: audio.clock_rate(),
            channels: u32::from(audio.channels().get()),
            codec_private: audio.extra_data().to_vec(),
        }),
        ParametersRef::Message(_) => None,
    }
}
