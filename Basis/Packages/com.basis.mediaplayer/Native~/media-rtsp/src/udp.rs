//! UDP transport: retina keeps the RTSP state machine (DESCRIBE/SETUP/
//! PLAY, keepalives, TEARDOWN, socket bind + connect + firewall punch)
//! and hands over the connected sockets and depacketizers; `media-rtp`
//! owns receive — reorder, jitter/loss accounting and RTCP receiver
//! reports, which retina's own UDP path lacks (servers kill RR-less
//! sessions as dead). The SETUP response's `source` address is vetted
//! before any packet goes out (§9.3): it is server-controlled and need
//! not match the RTSP host.

use std::collections::VecDeque;
use std::net::IpAddr;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Instant;

use futures::StreamExt;
use media_clock::{Generation, MediaTime};
use media_demux::{StreamEvent, TrackId};
use media_rtp::{ReceiverConfig, RtpReceiver};
use retina::client::{PlayOptions, SessionOptions, SetupOptions, Transport, UdpTransportOptions};
use retina::codec::{CodecItem, Depacketizer};
use retina::rtp::ReceivedPacketBuilder;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;

use crate::{
    ALIGN_WAIT, CHANNEL_DEPTH, MAX_STREAMS, PendingFrame, StreamAlign, emit, flush_aligned,
    format_from, frame_format, select_streams, send_event,
};

/// Peer-address policy for UDP targets, backed by the engine's one
/// address gate.
pub type UdpPeerAllowed = Arc<dyn Fn(IpAddr) -> bool + Send + Sync>;

pub(crate) struct UdpStream {
    index: usize,
    clock_rate: NonZeroU32,
    rtp: Arc<UdpSocket>,
    rtcp: Arc<UdpSocket>,
    receiver: RtpReceiver,
    depacketizer: Depacketizer,
    /// First delivered packet's unwrapped RTP timestamp; every later
    /// timestamp is rebased so stream elapsed starts at zero.
    ts_start: Option<i64>,
    announced: bool,
}

pub(crate) struct UdpReady {
    session: retina::client::Session<retina::client::Playing>,
    streams: Vec<UdpStream>,
    video_index: Option<usize>,
    audio_index: Option<usize>,
}

/// Derive a local SSRC for our receiver reports. Uniqueness matters
/// only within the session; a hash of the stream index seeded by the
/// process's `RandomState` is plenty.
fn local_ssrc(index: usize) -> u32 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_usize(index);
    (hasher.finish() & 0xFFFF_FFFF) as u32
}

pub(crate) async fn setup_udp_session(
    url: url::Url,
    peer_allowed: UdpPeerAllowed,
) -> Result<UdpReady, String> {
    let options = SessionOptions::default()
        .user_agent("basis-media".into())
        .udp_peer_validator(Arc::new(move |ip| peer_allowed(ip)));
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
                    .transport(Transport::Udp(UdpTransportOptions::default()))
                    .frame_format(frame_format()),
            )
            .await
            .map_err(|e| format!("rtsp setup (udp): {e}"))?;
    }

    let mut session = session
        .play(
            PlayOptions::default()
                .initial_timestamp(retina::client::InitialTimestampPolicy::Permissive),
        )
        .await
        .map_err(|e| format!("rtsp play: {e}"))?;

    let mut streams = Vec::new();
    for index in [video_index, audio_index].into_iter().flatten() {
        let (rtp, rtcp) = session
            .take_udp_sockets(index)
            .ok_or("udp sockets missing after play")?;
        let depacketizer = session
            .take_depacketizer(index)
            .ok_or("stream cannot be depacketized")?;
        let clock_rate = NonZeroU32::new(session.streams()[index].clock_rate_hz())
            .ok_or("stream advertises a zero clock rate")?;
        streams.push(UdpStream {
            index,
            clock_rate,
            rtp: Arc::new(rtp),
            rtcp: Arc::new(rtcp),
            receiver: RtpReceiver::new(ReceiverConfig::new(
                clock_rate.get(),
                local_ssrc(index),
                "basis-media",
            )),
            depacketizer,
            ts_start: None,
            announced: false,
        });
    }
    Ok(UdpReady {
        session,
        streams,
        video_index,
        audio_index,
    })
}

pub(crate) async fn run_udp_session(
    ready: UdpReady,
    generation: Generation,
    tx: &mpsc::Sender<Result<StreamEvent, String>>,
    mut first_datagram: Option<oneshot::Sender<()>>,
) -> Result<(), String> {
    let UdpReady {
        mut session,
        mut streams,
        video_index,
        audio_index,
    } = ready;
    let needed: Vec<usize> = [video_index, audio_index].into_iter().flatten().collect();

    // Announce formats known from the SDP up front.
    for stream in &mut streams {
        if let Some(parameters) = stream.depacketizer.parameters()
            && let Some(format) = format_from(&parameters)
        {
            send_event(
                tx,
                StreamEvent::Format(TrackId(stream.index as u32), format),
            )
            .await?;
            stream.announced = true;
        }
    }

    // One receive task per socket, funneling into a single channel; the
    // JoinSet aborts them all when the driver ends.
    let (dtx, mut drx) = mpsc::channel::<(usize, bool, Vec<u8>)>(512);
    let mut recv_tasks = JoinSet::new();
    for stream in &streams {
        for (is_rtcp, socket) in [(false, &stream.rtp), (true, &stream.rtcp)] {
            let dtx = dtx.clone();
            let socket = Arc::clone(socket);
            let index = stream.index;
            recv_tasks.spawn(async move {
                let mut buf = vec![0u8; 65_536];
                loop {
                    match socket.recv(&mut buf).await {
                        Ok(n) => {
                            if dtx.send((index, is_rtcp, buf[..n].to_vec())).await.is_err() {
                                break;
                            }
                        }
                        // Windows surfaces ICMP port-unreachable for
                        // previously sent packets (hole punch, RRs) as
                        // a receive error; the socket is fine.
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::ConnectionRefused
                            ) => {}
                        Err(_) => break,
                    }
                }
            });
        }
    }
    drop(dtx);

    let epoch = Instant::now();
    let now_of = |epoch: &Instant| MediaTime::from_micros(epoch.elapsed().as_micros() as i64);

    let mut align = [StreamAlign::default(); MAX_STREAMS];
    let mut buffered: VecDeque<PendingFrame> = VecDeque::new();
    let mut aligning = true;
    let align_deadline = tokio::time::Instant::now() + ALIGN_WAIT;

    loop {
        // Next reorder-gap release or receiver report across streams,
        // plus the alignment flush while it is pending.
        let mut wake: Option<MediaTime> = streams
            .iter()
            .filter_map(|s| s.receiver.next_deadline())
            .min();
        let wake_instant = {
            let media_wake = wake.map(|deadline| {
                let now = now_of(&epoch);
                let delta = deadline.saturating_sub(now).as_micros().max(0) as u64;
                tokio::time::Instant::now() + std::time::Duration::from_micros(delta)
            });
            let align_wake = if aligning { Some(align_deadline) } else { None };
            match (media_wake, align_wake) {
                (Some(a), Some(b)) => {
                    wake = Some(MediaTime::ZERO); // marker: a wake exists
                    Some(a.min(b))
                }
                (a, b) => {
                    let chosen = a.or(b);
                    if chosen.is_some() {
                        wake = Some(MediaTime::ZERO);
                    }
                    chosen
                }
            }
        };
        let far_future = tokio::time::Instant::now() + std::time::Duration::from_secs(3_600);

        tokio::select! {
            datagram = drx.recv() => {
                let now = now_of(&epoch);
                match datagram {
                    Some((index, is_rtcp, data)) => {
                        if let Some(first) = first_datagram.take() {
                            let _ = first.send(());
                        }
                        let stream = streams
                            .iter_mut()
                            .find(|s| s.index == index)
                            .expect("datagram for unknown stream");
                        if is_rtcp {
                            let _ = stream.receiver.on_rtcp(now, &data);
                            if stream.receiver.bye_received() {
                                // Sender said goodbye: the session is
                                // over (publisher stopped); surface as
                                // source loss so live lanes reconnect.
                                return Ok(());
                            }
                            update_alignment(stream, &mut align);
                            if aligning
                                && needed.iter().all(|&i| align[i].ntp_at_zero.is_some())
                            {
                                flush_aligned(&mut buffered, &mut align, &needed, generation, tx)
                                    .await?;
                                aligning = false;
                            }
                        } else {
                            let _ = stream.receiver.on_rtp(now, &data);
                            drain_stream(stream, now, aligning, &mut buffered, &align, generation, tx)
                                .await?;
                        }
                    }
                    None => return Err("udp receive tasks ended".into()),
                }
            }
            item = session.next() => {
                match item {
                    // Control connection closed by the server.
                    None => return Ok(()),
                    Some(Err(e)) => return Err(format!("rtsp session: {e}")),
                    // Stray interleaved items; media flows on our sockets.
                    Some(Ok(_)) => {}
                }
            }
            _ = tokio::time::sleep_until(wake_instant.unwrap_or(far_future)), if wake.is_some() => {
                let now = now_of(&epoch);
                for stream in streams.iter_mut() {
                    drain_stream(stream, now, aligning, &mut buffered, &align, generation, tx)
                        .await?;
                    if let Some(report) = stream.receiver.poll_rtcp_out(now) {
                        let _ = stream.rtcp.send(&report).await;
                    }
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

/// Release everything the reorder buffer will give up at `now` and feed
/// it through the depacketizer into the emit path.
async fn drain_stream(
    stream: &mut UdpStream,
    now: MediaTime,
    aligning: bool,
    buffered: &mut VecDeque<PendingFrame>,
    align: &[StreamAlign; MAX_STREAMS],
    generation: Generation,
    tx: &mpsc::Sender<Result<StreamEvent, String>>,
) -> Result<(), String> {
    while let Some(packet) = stream.receiver.poll_packet(now) {
        let start = *stream.ts_start.get_or_insert(packet.timestamp);
        // Pre-join edge: a reordered predecessor of the first delivered
        // packet has nowhere to go on a zero-based timeline.
        if packet.timestamp < start {
            continue;
        }
        let Some(timestamp) =
            retina::Timestamp::new(packet.timestamp - start, stream.clock_rate, 0)
        else {
            continue;
        };
        let built = ReceivedPacketBuilder {
            ctx: retina::PacketContext::dummy(),
            stream_id: stream.index,
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
        if stream.depacketizer.push(built).is_err() {
            continue;
        }
        while let Some(item) = stream.depacketizer.pull() {
            let Ok(item) = item else {
                continue;
            };
            match item {
                CodecItem::VideoFrame(frame) => {
                    if (frame.has_new_parameters() || !stream.announced)
                        && let Some(parameters) = stream.depacketizer.parameters()
                        && let Some(format) = format_from(&parameters)
                    {
                        send_event(
                            tx,
                            StreamEvent::Format(TrackId(stream.index as u32), format),
                        )
                        .await?;
                        stream.announced = true;
                    }
                    let ts = frame.timestamp();
                    let elapsed_us = ts.elapsed() * 1_000_000 / i64::from(ts.clock_rate().get());
                    let pending = PendingFrame {
                        stream_id: stream.index,
                        elapsed_us,
                        key: frame.is_random_access_point(),
                        data: frame.into_data(),
                    };
                    emit(pending, aligning, buffered, align, generation, tx).await?;
                }
                CodecItem::AudioFrame(frame) => {
                    if !stream.announced
                        && let Some(parameters) = stream.depacketizer.parameters()
                        && let Some(format) = format_from(&parameters)
                    {
                        send_event(
                            tx,
                            StreamEvent::Format(TrackId(stream.index as u32), format),
                        )
                        .await?;
                        stream.announced = true;
                    }
                    let ts = frame.timestamp();
                    let elapsed_us = ts.elapsed() * 1_000_000 / i64::from(ts.clock_rate().get());
                    let pending = PendingFrame {
                        stream_id: stream.index,
                        elapsed_us,
                        key: true,
                        data: frame.data().to_vec(),
                    };
                    emit(pending, aligning, buffered, align, generation, tx).await?;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// NTP↔RTP alignment from the stream's latest sender report, on the
/// same zero-based timeline as the emitted frames.
fn update_alignment(stream: &mut UdpStream, align: &mut [StreamAlign; MAX_STREAMS]) {
    if stream.index >= MAX_STREAMS || align[stream.index].ntp_at_zero.is_some() {
        return;
    }
    let (Some(sr), Some(start)) = (stream.receiver.sender_info(), stream.ts_start) else {
        return;
    };
    let ext = stream.receiver.extend_report_ts(sr.rtp_timestamp);
    let elapsed_us = (ext - start) * 1_000_000 / i64::from(stream.clock_rate.get());
    let elapsed_ntp = (elapsed_us as f64 / 1e6) * 4_294_967_296.0;
    align[stream.index].ntp_at_zero = Some(if elapsed_ntp >= 0.0 {
        sr.ntp.wrapping_sub(elapsed_ntp as u64)
    } else {
        sr.ntp.wrapping_add((-elapsed_ntp) as u64)
    });
}
