"""Regenerate the length-prefixed access units using FFmpeg's synthetic testsrc2."""
import json
from pathlib import Path
import struct
import subprocess
import tempfile

HERE = Path(__file__).resolve().parent
ENCODERS = {
    "h264": ["-c:v", "libx264", "-preset", "veryfast", "-tune", "zerolatency",
             "-x264-params", "threads=2:bframes=0:scenecut=0", "-f", "h264"],
    "hevc": ["-c:v", "libx265", "-preset", "ultrafast", "-tune", "zerolatency",
             "-x265-params", "pools=2:frame-threads=1:bframes=0:log-level=error", "-f", "hevc"],
    "av1": ["-c:v", "libaom-av1", "-usage", "realtime", "-cpu-used", "8",
            "-lag-in-frames", "0", "-row-mt", "1", "-threads", "4", "-f", "obu"],
}
with tempfile.TemporaryDirectory() as tmp:
    for width, height in [(1280, 720), (1920, 1080), (3840, 2160)]:
        for codec, opts in ENCODERS.items():
            raw = Path(tmp) / codec
            subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i",
                            f"testsrc2=size={width}x{height}:rate=60", "-frames:v", "12",
                            "-pix_fmt", "yuv420p", "-g", "60", "-b:v", "0", "-crf", "32",
                            *opts, str(raw)], check=True)
            packets = json.loads(subprocess.check_output(["ffprobe", "-v", "error", "-show_packets",
                                 "-show_entries", "packet=pos,size,flags", "-of", "json", str(raw)]))["packets"]
            assert len(packets) == 12 and "K" in packets[0]["flags"]
            data = raw.read_bytes()
            with (HERE / f"{height}.{codec}.au").open("wb") as out:
                for pkt in packets:
                    start, size = int(pkt["pos"]), int(pkt["size"])
                    out.write(struct.pack(">I", size))
                    out.write(data[start:start + size])
