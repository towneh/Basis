//! The WebRTC receive session driver: one UDP socket and one sans-IO
//! [`Rtc`] state machine on the shared I/O runtime. str0m owns ICE,
//! DTLS, SRTP and RTCP (receiver reports, NACK); decrypted RTP hands off
//! to media-rtp's reorder/jitter layer, retina's depacketizers (H.264)
//! or the trivial Opus mapping (RFC 7587: one packet, one frame), and
//! media-rtsp's shared aligner/emit path — WHEP frames enter the engine
//! exactly where RTSP frames do.
//!
//! §9.3 enforcement point: every datagram str0m wants to send passes
//! the engine's address gate first. Remote ICE candidates are
//! server-controlled (SDP and trickle alike), so the check sits at the
//! transmit boundary where it cannot be bypassed — a blocked candidate's
//! connectivity check is never sent, which is exactly "validated before
//! any check goes out". Inbound traffic is safe to parse from anywhere:
//! ICE consent requires message integrity with session credentials.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use bytes::Bytes;
use media_clock::{Generation, MediaTime};
use media_demux::{AudioCodec, Format, StreamEvent, TrackId};
use media_io::AddressGate;
use media_rtp::{ReceiverConfig, RtpFields, RtpReceiver};
use media_rtsp::{
    ALIGN_WAIT, CHANNEL_DEPTH, MAX_STREAMS, PendingFrame, StreamAlign, emit, flush_aligned,
    format_from, frame_format, send_event,
};
use retina::codec::{CodecItem, Depacketizer};
use retina::rtp::ReceivedPacketBuilder;
use str0m::media::MediaTime as StrMediaTime;
use str0m::net::{Protocol, Receive};
use str0m::rtp::rtcp::SenderInfo;
use str0m::{Event, IceConnectionState, Input, Output, Rtc};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

/// Stable stream ids on the emit path (WebRTC mids are strings; the
/// engine wants small track indices).
pub(crate) const VIDEO_STREAM: usize = 0;
pub(crate) const AUDIO_STREAM: usize = 1;

#[derive(Debug, Clone, Copy)]
pub(crate) enum LaneCodec {
    H264,
    Opus { channels: u8 },
}

/// One receive lane: media-rtp reorder + depacketisation state.
pub(crate) struct Lane {
    pub(crate) stream_id: usize,
    pub(crate) codec: LaneCodec,
    /// Negotiated payload types routing to this lane (a codec can
    /// negotiate several — H.264 profiles).
    pub(crate) pts: Vec<u8>,
    pub(crate) clock_rate: NonZeroU32,
    pub(crate) receiver: RtpReceiver,
    /// H.264 goes through retina's depacketizer; Opus needs none.
    pub(crate) depacketizer: Option<Depacketizer>,
    /// First delivered packet's unwrapped RTP timestamp; stream elapsed
    /// starts at zero there.
    pub(crate) ts_start: Option<i64>,
    pub(crate) last_sr: Option<SenderInfo>,
    pub(crate) announced: bool,
}

impl Lane {
    pub(crate) fn new(
        stream_id: usize,
        codec: LaneCodec,
        pts: Vec<u8>,
        clock_rate: u32,
    ) -> Result<Self, String> {
        let clock_rate =
            NonZeroU32::new(clock_rate).ok_or("negotiated codec with zero clock rate")?;
        let depacketizer = match codec {
            LaneCodec::H264 => {
                let mut depacketizer =
                    Depacketizer::new("video", "h264", clock_rate.get(), None, None)
                        .map_err(|e| format!("h264 depacketizer: {e}"))?;
                depacketizer.set_frame_format(frame_format());
                Some(depacketizer)
            }
            LaneCodec::Opus { .. } => None,
        };
        Ok(Self {
            stream_id,
            codec,
            pts,
            clock_rate,
            receiver: RtpReceiver::new(ReceiverConfig::new(
                clock_rate.get(),
                // Reports are never sent (str0m owns RTCP); the local
                // SSRC only labels internal state.
                stream_id as u32 + 1,
                "basis-media",
            )),
            depacketizer,
            ts_start: None,
            last_sr: None,
            announced: false,
        })
    }
}

/// A negotiated session ready for the run loop.
pub(crate) struct SessionReady {
    pub(crate) rtc: Rtc,
    pub(crate) socket: std::net::UdpSocket,
    pub(crate) local_addr: SocketAddr,
    pub(crate) gate: Arc<dyn AddressGate>,
    pub(crate) lanes: Vec<Lane>,
}

/// Unix-epoch-based NTP 32.32. The aligner only uses differences
/// between streams, so the constant NTP-era offset cancels.
fn ntp64(t: SystemTime) -> u64 {
    let d = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    (d.as_secs() << 32) | (u64::from(d.subsec_nanos()) * (1 << 32) / 1_000_000_000)
}

pub(crate) async fn run_session(
    ready: SessionReady,
    generation: Generation,
    tx: &mpsc::Sender<Result<StreamEvent, String>>,
    mut first_frame: Option<tokio::sync::oneshot::Sender<()>>,
) -> Result<(), String> {
    let SessionReady {
        mut rtc,
        socket,
        local_addr,
        gate,
        mut lanes,
    } = ready;
    let socket = UdpSocket::from_std(socket).map_err(|e| format!("udp socket: {e}"))?;
    let needed: Vec<usize> = lanes.iter().map(|l| l.stream_id).collect();

    let epoch = Instant::now();
    let now_media = |epoch: &Instant| MediaTime::from_micros(epoch.elapsed().as_micros() as i64);

    let mut align = [StreamAlign::default(); MAX_STREAMS];
    let mut buffered: VecDeque<PendingFrame> = VecDeque::new();
    let mut aligning = true;
    let align_deadline = tokio::time::Instant::now() + ALIGN_WAIT;
    // One note per blocked address; a hostile candidate list must not
    // flood stderr (§10).
    let mut blocked_peers: Vec<std::net::IpAddr> = Vec::new();

    let mut buf = vec![0u8; 65_536];
    loop {
        // Drive str0m until it reports a timeout, sending datagrams and
        // handling events as they surface.
        let str0m_deadline = loop {
            match rtc.poll_output().map_err(|e| format!("webrtc: {e}"))? {
                Output::Timeout(t) => break t,
                Output::Transmit(t) => {
                    if gate.permit(t.destination.ip()) {
                        let _ = socket.send_to(&t.contents, t.destination).await;
                    } else if !blocked_peers.contains(&t.destination.ip()) {
                        blocked_peers.push(t.destination.ip());
                        eprintln!(
                            "[basis-media] whep: blocked candidate address {} (gate)",
                            t.destination.ip()
                        );
                    }
                }
                Output::Event(event) => match event {
                    Event::RtpPacket(packet) => {
                        let now = now_media(&epoch);
                        if let Some(lane) = lanes
                            .iter_mut()
                            .find(|l| lane_matches(l, *packet.header.payload_type))
                        {
                            if lane.last_sr.is_none() {
                                lane.last_sr = packet.last_sender_info;
                            }
                            let _ = lane.receiver.on_packet(
                                now,
                                RtpFields {
                                    seq: packet.header.sequence_number,
                                    timestamp: packet.header.timestamp,
                                    ssrc: *packet.header.ssrc,
                                    payload_type: *packet.header.payload_type,
                                    marker: packet.header.marker,
                                },
                                Bytes::copy_from_slice(&packet.payload),
                            );
                            drain_lane(lane, now, aligning, &mut buffered, &align, generation, tx)
                                .await?;
                            if let Some(first) = first_frame.take() {
                                let _ = first.send(());
                            }
                            update_alignment(lane, &mut align);
                            if aligning && needed.iter().all(|&i| align[i].ntp_at_zero.is_some()) {
                                flush_aligned(&mut buffered, &mut align, &needed, generation, tx)
                                    .await?;
                                aligning = false;
                            }
                        }
                    }
                    Event::IceConnectionStateChange(state) => {
                        eprintln!("[basis-media] whep ice: {state:?}");
                        if state == IceConnectionState::Disconnected {
                            // The media path died (publisher gone, NAT
                            // rebind): surface as source loss so the
                            // engine's reconnect path re-signals.
                            return Ok(());
                        }
                    }
                    Event::Connected => {
                        eprintln!("[basis-media] whep: connected");
                    }
                    _ => {}
                },
            }
        };

        // Reorder-gap releases wake independently of network traffic.
        let rtp_deadline = lanes
            .iter()
            .filter_map(|l| l.receiver.next_deadline())
            .min()
            .map(|deadline| {
                let now = now_media(&epoch);
                let delta = deadline.saturating_sub(now).as_micros().max(0) as u64;
                tokio::time::Instant::now() + std::time::Duration::from_micros(delta)
            });
        let mut wake = tokio::time::Instant::from_std(str0m_deadline);
        if let Some(deadline) = rtp_deadline {
            wake = wake.min(deadline);
        }
        if aligning {
            wake = wake.min(align_deadline);
        }

        tokio::select! {
            received = socket.recv_from(&mut buf) => {
                match received {
                    Ok((n, source)) => {
                        if let Ok(contents) = buf[..n].try_into() {
                            let input = Input::Receive(
                                Instant::now(),
                                Receive {
                                    proto: Protocol::Udp,
                                    source,
                                    destination: local_addr,
                                    contents,
                                },
                            );
                            let _ = rtc.handle_input(input);
                        }
                    }
                    // Windows surfaces ICMP port-unreachable for earlier
                    // sends as a receive error; the socket is fine.
                    Err(e) if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionRefused
                    ) => {}
                    Err(e) => return Err(format!("udp receive: {e}")),
                }
            }
            _ = tokio::time::sleep_until(wake) => {
                let _ = rtc.handle_input(Input::Timeout(Instant::now()));
                let now = now_media(&epoch);
                for lane in lanes.iter_mut() {
                    drain_lane(lane, now, aligning, &mut buffered, &align, generation, tx).await?;
                }
                if aligning && tokio::time::Instant::now() >= align_deadline {
                    flush_aligned(&mut buffered, &mut align, &needed, generation, tx).await?;
                    aligning = false;
                }
            }
        }

        if aligning && buffered.len() >= CHANNEL_DEPTH {
            flush_aligned(&mut buffered, &mut align, &needed, generation, tx).await?;
            aligning = false;
        }
    }
}

fn lane_matches(lane: &Lane, pt: u8) -> bool {
    lane.pts.contains(&pt)
}

/// Release everything the reorder buffer will give up at `now` and feed
/// it through depacketisation into the shared emit path.
async fn drain_lane(
    lane: &mut Lane,
    now: MediaTime,
    aligning: bool,
    buffered: &mut VecDeque<PendingFrame>,
    align: &[StreamAlign; MAX_STREAMS],
    generation: Generation,
    tx: &mpsc::Sender<Result<StreamEvent, String>>,
) -> Result<(), String> {
    while let Some(packet) = lane.receiver.poll_packet(now) {
        let start = *lane.ts_start.get_or_insert(packet.timestamp);
        // Pre-join edge: a reordered predecessor of the first delivered
        // packet has nowhere to go on a zero-based timeline.
        if packet.timestamp < start {
            continue;
        }
        match &mut lane.depacketizer {
            None => {
                // Opus (RFC 7587): one RTP packet is one frame.
                let LaneCodec::Opus { channels } = lane.codec else {
                    unreachable!("only opus lanes run without a depacketizer");
                };
                if !lane.announced {
                    send_event(
                        tx,
                        StreamEvent::Format(
                            TrackId(lane.stream_id as u32),
                            Format::Audio {
                                codec: AudioCodec::Opus,
                                sample_rate: lane.clock_rate.get(),
                                channels: u32::from(channels),
                                codec_private: opus_head(channels, lane.clock_rate.get()),
                            },
                        ),
                    )
                    .await?;
                    lane.announced = true;
                }
                let elapsed_us =
                    (packet.timestamp - start) * 1_000_000 / i64::from(lane.clock_rate.get());
                let pending = PendingFrame {
                    stream_id: lane.stream_id,
                    elapsed_us,
                    key: true,
                    data: packet.payload.to_vec(),
                };
                emit(pending, aligning, buffered, align, generation, tx).await?;
            }
            Some(depacketizer) => {
                let Some(timestamp) =
                    retina::Timestamp::new(packet.timestamp - start, lane.clock_rate, 0)
                else {
                    continue;
                };
                let built = ReceivedPacketBuilder {
                    ctx: retina::PacketContext::dummy(),
                    stream_id: lane.stream_id,
                    sequence_number: (packet.seq & 0xFFFF) as u16,
                    timestamp,
                    payload_type: packet.payload_type,
                    ssrc: packet.ssrc,
                    mark: packet.marker,
                    loss: packet.loss,
                }
                .build(packet.payload)
                .map_err(|e| format!("rebuild rtp packet: {e}"))?;

                // Per-packet depacketizer refusals are loss, not session
                // failure — this lane tolerates loss by design.
                if depacketizer.push(built).is_err() {
                    continue;
                }
                while let Some(item) = depacketizer.pull() {
                    let Ok(item) = item else {
                        continue;
                    };
                    if let CodecItem::VideoFrame(frame) = item {
                        if (frame.has_new_parameters() || !lane.announced)
                            && let Some(parameters) = depacketizer.parameters()
                            && let Some(format) = format_from(&parameters)
                        {
                            send_event(
                                tx,
                                StreamEvent::Format(TrackId(lane.stream_id as u32), format),
                            )
                            .await?;
                            lane.announced = true;
                        }
                        let ts = frame.timestamp();
                        let elapsed_us =
                            ts.elapsed() * 1_000_000 / i64::from(ts.clock_rate().get());
                        let pending = PendingFrame {
                            stream_id: lane.stream_id,
                            elapsed_us,
                            key: frame.is_random_access_point(),
                            data: frame.into_data(),
                        };
                        emit(pending, aligning, buffered, align, generation, tx).await?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Minimal OpusHead (the decode adapter's codec_private contract):
/// WebRTC Opus carries no container header, so state the negotiated
/// channel count with zero pre-skip.
fn opus_head(channels: u8, input_rate: u32) -> Vec<u8> {
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1); // version
    head.push(channels);
    head.extend_from_slice(&0u16.to_le_bytes()); // pre-skip
    head.extend_from_slice(&input_rate.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes()); // output gain
    head.push(0); // mapping family 0 (mono/stereo)
    head
}

/// NTP↔RTP alignment from the stream's latest sender report, on the
/// same zero-based timeline as the emitted frames. str0m surfaces the
/// SR with each packet; the raw 32-bit RTP time is extended against the
/// receiver's own timeline exactly as the RTSP-UDP driver does.
fn update_alignment(lane: &mut Lane, align: &mut [StreamAlign; MAX_STREAMS]) {
    if lane.stream_id >= MAX_STREAMS || align[lane.stream_id].ntp_at_zero.is_some() {
        return;
    }
    let (Some(sr), Some(start)) = (lane.last_sr.as_ref(), lane.ts_start) else {
        return;
    };
    let raw = sr_rtp_raw(sr.rtp_time);
    let ext = lane.receiver.extend_report_ts(raw);
    let elapsed_us = (ext - start) * 1_000_000 / i64::from(lane.clock_rate.get());
    let elapsed_ntp = (elapsed_us as f64 / 1e6) * 4_294_967_296.0;
    let ntp = ntp64(sr.ntp_time);
    align[lane.stream_id].ntp_at_zero = Some(if elapsed_ntp >= 0.0 {
        ntp.wrapping_sub(elapsed_ntp as u64)
    } else {
        ntp.wrapping_add((-elapsed_ntp) as u64)
    });
}

/// The low 32 bits of str0m's extended SR RTP time are the wire value.
fn sr_rtp_raw(t: StrMediaTime) -> u32 {
    t.numer() as u32
}
