# Vendored retina 0.4.19

Vendored copy of the `retina` crate (MIT OR Apache-2.0, see README.md),
applied via `[patch.crates-io]` in the workspace root. Benches and lockfile
dropped; source otherwise identical to the crates.io release except:

- `src/client/rtp.rs`, `InorderParser::new`: an all-zero SSRC from the
  SETUP/PLAY response headers is treated as unstated instead of pinned.
  Some servers (VRCDN's RTSP edge among them) advertise `ssrc=00000000`
  as a placeholder while the RTP stream carries a real SSRC; pinning the
  placeholder rejects every packet with "wrong ssrc".
- `src/codec/aac.rs`, `pull`: a complete AAC AU whose packet lacks the
  RTP marker bit is accepted instead of failing the session. RFC 3640
  wants the mark set, but mediamtx's RTSP output omits it on these
  packets and mainstream clients tolerate that.
- `src/codec/aac.rs`, `push`: the body moved to `push_inner` and the
  public entry point discards an in-progress reassembly whenever it
  returns an error. Upstream leaves the fragment state intact on every
  refusal, which is harmless under retina's own interleaved receive loop
  because that turns a refusal into a session error. A UDP loop that
  treats refusals as recoverable loss and keeps feeding the same
  depacketizer instead has the next marked packet appended to a prefix
  the depacketizer already rejected, and emits it as a truncated access
  unit. Doing it at the boundary rather than at each refusal covers the
  header checks that run before the state is examined, and any refusal
  added later.

- `src/client/mod.rs`, `Session<Playing>`: two additions for callers
  driving UDP receive outside the session — `take_udp_sockets(i)` hands
  over a stream's connected RTP/RTCP socket pair (the session stops
  polling them; keepalives and the control connection are unaffected)
  and `take_depacketizer(i)` hands over the stream's depacketizer with
  SDP parameters and frame format applied. Retina's own UDP receive
  path has no reorder buffer and sends no RTCP receiver reports, which
  real servers kill sessions over; these seams let an external
  reorder/RTCP layer reuse everything else.
- `src/client/mod.rs`, `SessionOptions::udp_peer_validator` +
  `setup()`: an optional callback validating the UDP peer address
  before the sockets are connected or hole-punch packets are sent. The
  `Transport` header's `source` parameter is server-controlled, so
  without this an attacker-run RTSP server can direct UDP traffic at an
  arbitrary (e.g. internal) address — an SSRF channel for any client
  with an address policy.

- `src/lib.rs`, `UdpPair::for_ip`: the even/odd bind loop also retries
  on `PermissionDenied`, not just `AddrInUse`. Windows reports ports
  inside its excluded ephemeral ranges (Hyper-V/WinNAT reservations,
  `netsh interface ipv4 show excludedportrange`) as WSAEACCES →
  `PermissionDenied`, so without the retry a few percent of UDP setups
  fail outright on a stock Windows box.

All are candidate upstream reports/PRs to scottlamb/retina (the peer
validator especially, and the Windows bind retry is a straight bug
fix); drop the vendored copy when a release carries fixes or options.
