#!/usr/bin/env python3
"""Generate fixtures/h264-aac-late-video.mp4: an MP4 whose picture starts
half a second after its sound.

Three seconds of H.264 (320x180, 24 fps, two B-frames) and stereo AAC are
muxed into MPEG-TS with the video leg offset by 0.5 s, then stream-copied
into MP4. The MP4 muxer records the offset the way any remux of such a
source does: the video track's edit list opens with an empty edit
(`media_time` -1) as long as the gap, followed by the ordinary edit that
skips the B-frame reorder delay. A reader that ignores the empty edit
shows the picture early by the gap.

Needs ffmpeg + ffprobe on PATH. Run from Native~ (the fixture path is
relative to it):

    python tools/gen-late-video-fixture.py
"""

import os
import struct
import subprocess
import tempfile

FIXTURE = os.path.join("fixtures", "h264-aac-late-video.mp4")
SECONDS = 3
VIDEO_OFFSET = "0.5"
BITEXACT = ["-bitexact", "-fflags", "+bitexact"]


def video_edits(data):
    """The video track's edit list as (segment_duration, media_time) pairs."""
    def walk(pos, end):
        while pos + 8 <= end:
            size, kind = struct.unpack(">I4s", data[pos:pos + 8])
            assert size >= 8, f"bad box size at {pos}"
            if kind == b"trak":
                edits = []
                handler = None
                for inner in walk_trak(pos + 8, pos + size, edits):
                    handler = inner
                if handler == b"vide":
                    return edits
            elif kind == b"moov":
                found = walk(pos + 8, pos + size)
                if found is not None:
                    return found
            pos += size
        return None

    def walk_trak(pos, end, edits):
        while pos + 8 <= end:
            size, kind = struct.unpack(">I4s", data[pos:pos + 8])
            if kind in (b"edts", b"mdia"):
                yield from walk_trak(pos + 8, pos + size, edits)
            elif kind == b"elst":
                version = data[pos + 8]
                count = struct.unpack(">I", data[pos + 12:pos + 16])[0]
                p = pos + 16
                for _ in range(count):
                    if version == 1:
                        edits.append(struct.unpack(">Qq", data[p:p + 16]))
                        p += 20
                    else:
                        edits.append(struct.unpack(">Ii", data[p:p + 8]))
                        p += 12
            elif kind == b"hdlr":
                yield data[pos + 16:pos + 20]
            pos += size

    return walk(0, len(data))


def first_pts(path, stream):
    out = subprocess.run(
        ["ffprobe", "-v", "error", "-select_streams", stream,
         "-show_entries", "packet=pts_time", "-of", "csv=p=0", path],
        check=True, capture_output=True, text=True).stdout
    return min(float(line) for line in out.split())


def main():
    os.makedirs("fixtures", exist_ok=True)
    with tempfile.TemporaryDirectory() as scratch:
        legs = os.path.join(scratch, "legs.ts")
        subprocess.run(
            ["ffmpeg", "-nostdin", "-v", "error", "-y",
             "-itsoffset", VIDEO_OFFSET,
             "-f", "lavfi", "-i", f"testsrc2=duration={SECONDS}:size=320x180:rate=24",
             "-f", "lavfi", "-i", f"sine=frequency=440:duration={SECONDS}",
             "-c:v", "libx264", "-preset", "slow", "-crf", "32", "-g", "12",
             "-bf", "2", "-pix_fmt", "yuv420p",
             "-c:a", "aac", "-b:a", "64k", "-ar", "48000", "-ac", "2",
             *BITEXACT, "-f", "mpegts", legs], check=True)
        subprocess.run(
            ["ffmpeg", "-nostdin", "-v", "error", "-y", "-i", legs,
             "-c", "copy", *BITEXACT, FIXTURE], check=True)

    edits = video_edits(open(FIXTURE, "rb").read())
    assert edits and edits[0][1] == -1, f"no leading empty edit: {edits}"
    video, audio = first_pts(FIXTURE, "v"), first_pts(FIXTURE, "a")
    assert video - audio >= 0.4, f"video {video} s, audio {audio} s"
    print(f"wrote {FIXTURE}: video edits {edits}, "
          f"first pts video {video} s, audio {audio} s")


if __name__ == "__main__":
    main()
