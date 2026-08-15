#!/usr/bin/env python3
"""Windowed live-HLS server over the committed fixture segments.

Serves /live.m3u8 as a live playlist (no ENDLIST) whose window slides in
real time: three segments visible, media sequence advancing every target
duration. Segments cycle 0,1,2,0,1,2,... with #EXT-X-DISCONTINUITY at
every wrap (the timestamps restart, and the playlist says so). After
--duration seconds the playlist gains EXT-X-ENDLIST and the session ends
naturally.

Usage: live-hls-server.py <fixtures/hls/ts dir> <duration_s> <port>
"""

import http.server
import sys
import threading
import time

SEGMENT_SECONDS = 2.0
WINDOW = 3
SEGMENTS = ["seg000.ts", "seg001.ts", "seg002.ts"]


def main() -> None:
    root, duration_s, port = sys.argv[1], float(sys.argv[2]), int(sys.argv[3])
    start = time.monotonic()

    blobs = {}
    for name in SEGMENTS:
        with open(f"{root}/{name}", "rb") as f:
            blobs[name] = f.read()

    def playlist() -> bytes:
        elapsed = time.monotonic() - start
        ended = elapsed >= duration_s
        newest = int(min(elapsed, duration_s) / SEGMENT_SECONDS)
        first = max(0, newest - (WINDOW - 1))
        lines = [
            "#EXTM3U",
            "#EXT-X-VERSION:3",
            f"#EXT-X-TARGETDURATION:{int(SEGMENT_SECONDS)}",
            f"#EXT-X-MEDIA-SEQUENCE:{first}",
        ]
        for seq in range(first, newest + 1):
            if seq > 0 and seq % len(SEGMENTS) == 0:
                lines.append("#EXT-X-DISCONTINUITY")
            lines.append(f"#EXTINF:{SEGMENT_SECONDS:.3f},")
            lines.append(f"/{seq}/{SEGMENTS[seq % len(SEGMENTS)]}")
        if ended:
            lines.append("#EXT-X-ENDLIST")
        return ("\n".join(lines) + "\n").encode()

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):  # noqa: N802 (stdlib naming)
            if self.path == "/live.m3u8":
                body = playlist()
                ctype = "application/vnd.apple.mpegurl"
            else:
                name = self.path.rsplit("/", 1)[-1]
                if name not in blobs:
                    self.send_error(404)
                    return
                body = blobs[name]
                ctype = "video/mp2t"
            self.send_response(200)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *args):
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler)
    threading.Timer(duration_s + 30, server.shutdown).start()
    print(f"live HLS on http://127.0.0.1:{port}/live.m3u8 for {duration_s}s", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
