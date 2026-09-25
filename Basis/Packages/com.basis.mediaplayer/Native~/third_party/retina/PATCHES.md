# Vendored retina 0.4.19

A copy of the `retina` crate (MIT OR Apache-2.0, see README.md), applied through
`[patch.crates-io]` in the workspace root. The benches and lockfile are
dropped. The source matches the crates.io release apart from these changes:

- `src/client/rtp.rs`, `InorderParser::new`: an all-zero SSRC in the SETUP or
  PLAY response is treated as unstated rather than pinned. Some servers
  (VRCDN's RTSP edge among them) advertise `ssrc=00000000` while the RTP stream
  carries a real SSRC, and pinning the placeholder rejects every packet.
- `src/codec/aac.rs`, `pull`: a complete AAC access unit whose packet lacks the
  RTP marker bit is accepted rather than failing the session. RFC 3640 wants the
  marker set, but mediamtx omits it on these packets and common clients accept
  them.
- `src/codec/aac.rs`, `push`: the body moved to `push_inner`, and the public
  entry point discards any reassembly in progress when it returns an error.
  Upstream keeps the fragment state across a refusal, which is harmless in
  retina's own receive loop because a refusal ends the session. A UDP loop that
  treats refusals as loss and keeps feeding the depacketizer would otherwise
  append the next packet to a prefix already rejected and emit a truncated
  access unit. After a refusal the depacketizer is `Idle`, carries the loss
  count forward and marks the next access unit as damaged: one completed by
  reassembly at that marker is dropped, one arriving whole in a single packet
  is kept. Nothing is emitted with damage in it, so a consumer sees a gap.
  `media-rtsp/tests/aac_reassembly.rs` covers both cases.
- `src/client/mod.rs`, `Session<Playing>`: `take_udp_sockets(i)` hands over a
  stream's connected RTP and RTCP sockets (the session stops polling them;
  keepalives and the control connection carry on), and `take_depacketizer(i)`
  hands over the stream's depacketizer with its SDP parameters applied.
  Retina's UDP receive has no reorder buffer and sends no RTCP receiver
  reports, which servers end sessions over, so the engine does both itself.
- `src/client/mod.rs`, `SessionOptions::udp_peer_validator` and `setup()`: an
  optional callback that checks the UDP peer address before the sockets
  connect or send hole-punch packets. The server controls the `Transport`
  header's `source` parameter, so without it a hostile RTSP server could aim
  UDP traffic at any address, internal ones included.
- `src/lib.rs`, `UdpPair::for_ip`: the even/odd port bind loop retries on
  `PermissionDenied` as well as `AddrInUse`. Windows reports ports in its
  excluded ranges (`netsh interface ipv4 show excludedportrange`) as
  `WSAEACCES`, so without the retry a few percent of UDP set-ups fail on a
  stock Windows machine.
- `src/tokio.rs`, `Connection::from_stream` and `Codec::decode`, and
  `src/rtsp/parse.rs`, `Parser::feed_inner`: an RTSP message is capped at
  1 MiB, head and body together (`MAX_MESSAGE_BYTES`). Upstream builds the
  connection's parser with no limit and reserves the peer's `Content-Length`
  up front, so one reply header could ask for any amount of memory; a head of
  endless header lines, or one line that never ends, grew without limit too.
  `media-rtsp/tests/rtsp_message_bounds.rs` covers all three.

Each is a candidate for an upstream report or pull request to
scottlamb/retina. The copy can go once a release carries them.
