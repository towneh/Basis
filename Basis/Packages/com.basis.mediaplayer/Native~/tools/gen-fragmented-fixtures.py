#!/usr/bin/env python3
"""Generate the two many-fragment MP4 fixtures:

    fixtures/h264-aac-manyfrag-sidx.mp4   fragmented, with a segment index
    fixtures/h264-aac-manyfrag.mp4        the same streams, no index
    fixtures/h264-aac-longfrag-sidx.mp4   indexed, keyframes inside fragments

40 s of H.264 (320x180, 24 fps, GOP 12, two B-frames so pts and dts
differ) + stereo AAC, cut into a fragment every 100 ms and at every
keyframe: about 400 `moof`+`mdat` pairs in a little over a megabyte.

`+default_base_moof` is required: without it ffmpeg writes `trun` data
offsets a reader cannot resolve per fragment. `+global_sidx` writes one
`sidx` per track ahead of the first fragment; ffmpeg marks every
reference as starting with a SAP whether or not the fragment opens on a
keyframe (most here do not), so a reader has to check the samples. The
last fragment carries audio only, which leaves the video track's index
short of the end of the file while the audio track's reaches it.

The second file is a stream copy of the first, so the two hold identical
samples and differ only in the index. Both end in an `mfra`.

The third is 20 s cut every 2 s without `+frag_keyframe`, so its
keyframes fall inside fragments rather than at their starts: a seek into
it lands part-way through a fragment, where the fragment's earlier
samples belong to the seek before it rather than after.

Needs ffmpeg on PATH. Run from Native~ (the fixture paths are relative
to it):

    python tools/gen-fragmented-fixtures.py
"""

import os
import struct
import subprocess
import sys

INDEXED = os.path.join("fixtures", "h264-aac-manyfrag-sidx.mp4")
PLAIN = os.path.join("fixtures", "h264-aac-manyfrag.mp4")
LONGFRAG = os.path.join("fixtures", "h264-aac-longfrag-sidx.mp4")
SECONDS = 40
LONGFRAG_SECONDS = 20
FRAGMENT = ["-frag_duration", "100000"]
BITEXACT = ["-bitexact", "-fflags", "+bitexact"]


def top_level(data):
    pos = 0
    while pos + 8 <= len(data):
        size, kind = struct.unpack(">I4s", data[pos:pos + 8])
        header = 8
        if size == 1:
            size = struct.unpack(">Q", data[pos + 8:pos + 16])[0]
            header = 16
        assert size >= header, f"bad box size at {pos}"
        yield kind.decode("latin-1"), pos, size
        pos += size
    assert pos == len(data), "boxes do not tile the file"


def sidx_reach(data, pos, size):
    """Where a `sidx` says its last referenced byte ends, and its count."""
    version = data[pos + 8]
    p = pos + 20
    if version == 0:
        first_offset = struct.unpack(">I", data[p + 4:p + 8])[0]
        p += 8
    else:
        first_offset = struct.unpack(">Q", data[p + 8:p + 16])[0]
        p += 16
    count = struct.unpack(">H", data[p + 2:p + 4])[0]
    p += 4
    total = 0
    for _ in range(count):
        word = struct.unpack(">I", data[p:p + 4])[0]
        assert word >> 31 == 0, "hierarchical reference"
        total += word & 0x7FFFFFFF
        p += 12
    return pos + size + first_offset + total, count


def check(path, want_index, min_fragments=257):
    subprocess.run(["ffmpeg", "-nostdin", "-v", "error", "-i", path,
                    "-f", "null", "-"], check=True)
    data = open(path, "rb").read()
    boxes = list(top_level(data))
    kinds = [kind for kind, _, _ in boxes]
    fragments = kinds.count("moof")
    assert fragments >= min_fragments, f"{path}: only {fragments} fragments"
    assert kinds[-1] == "mfra", f"{path}: no trailing mfra"
    media_end = boxes[-1][1]
    reaches = [sidx_reach(data, pos, size)
               for kind, pos, size in boxes if kind == "sidx"]
    if want_index:
        assert any(end == media_end for end, _ in reaches), \
            f"{path}: no sidx tiles to the mfra"
    else:
        assert not reaches, f"{path}: unexpected sidx"
    print(f"wrote {path}: {len(data)} bytes, {fragments} fragments, "
          f"sidx references {[count for _, count in reaches]}")


def main():
    os.makedirs("fixtures", exist_ok=True)
    subprocess.run(
        ["ffmpeg", "-nostdin", "-v", "error", "-y",
         "-f", "lavfi", "-i", f"testsrc2=duration={SECONDS}:size=320x180:rate=24",
         "-f", "lavfi", "-i", f"sine=frequency=440:duration={SECONDS}",
         "-c:v", "libx264", "-preset", "slow", "-crf", "32", "-g", "12",
         "-bf", "2", "-pix_fmt", "yuv420p",
         "-c:a", "aac", "-b:a", "64k", "-ar", "48000", "-ac", "2",
         "-movflags", "+frag_keyframe+empty_moov+default_base_moof+global_sidx",
         *FRAGMENT, *BITEXACT, INDEXED], check=True)
    subprocess.run(
        ["ffmpeg", "-nostdin", "-v", "error", "-y", "-i", INDEXED, "-c", "copy",
         "-movflags", "+frag_keyframe+empty_moov+default_base_moof",
         *FRAGMENT, *BITEXACT, PLAIN], check=True)
    subprocess.run(
        ["ffmpeg", "-nostdin", "-v", "error", "-y",
         "-f", "lavfi", "-i",
         f"testsrc2=duration={LONGFRAG_SECONDS}:size=320x180:rate=24",
         "-f", "lavfi", "-i", f"sine=frequency=440:duration={LONGFRAG_SECONDS}",
         "-c:v", "libx264", "-preset", "slow", "-crf", "32", "-g", "12",
         "-bf", "2", "-pix_fmt", "yuv420p",
         "-c:a", "aac", "-b:a", "64k", "-ar", "48000", "-ac", "2",
         "-movflags", "+empty_moov+default_base_moof+global_sidx",
         "-frag_duration", "2000000", *BITEXACT, LONGFRAG], check=True)
    check(INDEXED, want_index=True)
    check(PLAIN, want_index=False)
    check(LONGFRAG, want_index=True, min_fragments=8)
    return 0


if __name__ == "__main__":
    sys.exit(main())
