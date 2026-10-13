#!/usr/bin/env python3
"""生成 qy-codec-wav 的独立参考夹具（M0 T2）。

本脚本独立于 Rust 解码器：从已知整数样本直接构造 WAV 字节，
把样本值作为期望写入 .expect；对有效夹具再用 ffmpeg 解码为 s32le，
写入 .ffmpeg.s32le 并断言其等于 样本 << (32 - V)，作为第二个独立参考。

用法：python3 tools/gen_wav_fixtures.py [输出目录]
固定种子，重复运行产生逐字节相同的输出（ffmpeg 版本记录在 README）。
"""
import os
import struct
import subprocess
import sys

OUT = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
    os.path.dirname(__file__), "..", "crates", "qy-codec-wav", "tests", "fixtures")

SUBTYPE_PCM = bytes.fromhex("0100000000001000800000aa00389b71")
SUBTYPE_FLOAT = bytes.fromhex("0300000000001000800000aa00389b71")


class Lcg:
    def __init__(self, seed):
        self.s = seed & 0xFFFFFFFFFFFFFFFF

    def next(self):
        self.s = (self.s * 6364136223846793005 + 1442695040888963407) & 0xFFFFFFFFFFFFFFFF
        return self.s >> 11


def samples(seed, frames, ch, v):
    lo, hi = -(1 << (v - 1)), (1 << (v - 1)) - 1
    fixed = [hi, lo, 0, -1, 1, hi - 1, lo + 1]
    r = Lcg(seed)
    out = []
    for i in range(frames * ch):
        out.append(fixed[i] if i < len(fixed) else lo + r.next() % (hi - lo + 1))
    return out


def encode(vals, c, v, dirty_seed=None):
    r = Lcg(dirty_seed) if dirty_seed is not None else None
    b = bytearray()
    for s in vals:
        x = s << (c - v)
        if r is not None and c > v:
            x |= 1 + r.next() % ((1 << (c - v)) - 1)
        b += (x & ((1 << c) - 1)).to_bytes(c // 8, "little")
    return bytes(b)


def chunk(cid, body, pad=True):
    b = cid + struct.pack("<I", len(body)) + body
    if pad and len(body) % 2:
        b += b"\x00"
    return b


def fmt_pcm(ch, rate, bits, align=None, tag=1, size18=False):
    align = ch * (bits // 8) if align is None else align
    body = struct.pack("<HHIIHH", tag, ch, rate, rate * align, align, bits)
    if size18:
        body += struct.pack("<H", 0)
    return chunk(b"fmt ", body)


def fmt_ext(ch, rate, c, v, mask, sub=SUBTYPE_PCM, cb=22, extra=b"", align=None):
    align = ch * (c // 8) if align is None else align
    body = struct.pack("<HHIIHHHHI", 0xFFFE, ch, rate, rate * align, align, c, cb, v, mask) + sub + extra
    return chunk(b"fmt ", body)


def riff(*chunks, size_adjust=0, magic=b"RIFF", form=b"WAVE"):
    body = form + b"".join(chunks)
    return magic + struct.pack("<I", len(body) + size_adjust) + body


CASES = []


def case(name, data, result, fmt=None, vals=(), ch=1, v=None, mask=None, ffmpeg=False, c=None):
    CASES.append(dict(name=name, data=data, result=result, fmt=fmt, vals=list(vals),
                      ch=ch, v=v, mask=mask, ffmpeg=ffmpeg, c=c if c is not None else v))


def ok_pcm(name, ch, rate, bits, frames, seed, size18=False, extra_before=(), extra_after=()):
    vals = samples(seed, frames, ch, bits)
    data = riff(*extra_before, fmt_pcm(ch, rate, bits, size18=size18),
                chunk(b"data", encode(vals, bits, bits)), *extra_after)
    case(name, data, "ok", (rate, ch, bits), vals, ch, bits, None, True)


def ok_ext(name, ch, rate, c, v, mask, frames, seed, cb=22, extra=b"", dirty=None):
    vals = samples(seed, frames, ch, v)
    data = riff(fmt_ext(ch, rate, c, v, mask, cb=cb, extra=extra),
                chunk(b"data", encode(vals, c, v, dirty)))
    case(name, data, "ok", (rate, ch, v), vals, ch, v, mask, dirty is None, c)


# ---- 有效夹具 ----
ok_pcm("pcm16_stereo_44k", 2, 44100, 16, 300, 1)
ok_pcm("pcm16_mono_8k_fmt18", 1, 8000, 16, 257, 2, size18=True)
ok_pcm("pcm24_mono_48k_odd_list", 1, 48000, 24, 301, 3,
       extra_after=(chunk(b"LIST", b"INFOx"),))
ok_pcm("pcm32_stereo_96k", 2, 96000, 32, 200, 4)
ok_pcm("pcm24_stereo_large", 2, 88200, 24, 10000, 5)
ok_pcm("unknown_chunks_odd", 2, 44100, 16, 123, 6,
       extra_before=(chunk(b"JUNK", b"1234567"),),
       extra_after=())
# fmt 与 data 之间的未知奇数长度 chunk
_v = samples(7, 64, 2, 16)
case("chunk_between_fmt_data",
     riff(fmt_pcm(2, 48000, 16), chunk(b"qyX1", b"abcdefghi"), chunk(b"data", encode(_v, 16, 16))),
     "ok", (48000, 2, 16), _v, 2, 16, None, True)
ok_ext("ext16_mono_mask0", 1, 44100, 16, 16, 0, 100, 8)
ok_ext("ext12in16_stereo", 2, 48000, 16, 12, 3, 150, 9)
ok_ext("ext20in24_mono_center", 1, 96000, 24, 20, 4, 151, 10)
ok_ext("ext24in32_stereo_192k", 2, 192000, 32, 24, 3, 500, 11)
ok_ext("ext32_stereo", 2, 176400, 32, 32, 3, 99, 12)
ok_ext("ext24_stereo_cb24", 2, 48000, 24, 24, 3, 77, 13, cb=24, extra=b"\x00\x00")
ok_ext("ext20in24_dirty_pad_bits", 2, 48000, 24, 20, 3, 80, 14, dirty=99)
case("empty_data", riff(fmt_pcm(2, 44100, 16), chunk(b"data", b"")), "ok", (44100, 2, 16), [], 2, 16)
# 最后一个奇数长度 chunk 缺少填充字节，RIFF 大小也未计入填充：容忍。
_v = samples(15, 33, 1, 24)
case("final_pad_missing",
     riff(fmt_pcm(1, 44100, 24), chunk(b"data", encode(_v, 24, 24), pad=False)),
     "ok", (44100, 1, 24), _v, 1, 24, None, True)
_v = samples(16, 20, 2, 16)
case("trailing_bytes_after_riff",
     riff(fmt_pcm(2, 44100, 16), chunk(b"data", encode(_v, 16, 16))) + b"garbage!",
     "ok", (44100, 2, 16), _v, 2, 16)

# ---- 截断 ----
_v = samples(20, 100, 2, 16)
_full = riff(fmt_pcm(2, 44100, 16), chunk(b"data", encode(_v, 16, 16)))
_cut = 44 + 50 * 4 + 3  # 第 51 帧中间
case("trunc_mid_frame", _full[:_cut], "Truncated", (44100, 2, 16), _v[:100], 2, 16)
case("trunc_in_fmt", _full[:20], "Truncated")
case("trunc_riff_header", _full[:7], "Truncated")
_v = samples(21, 10, 1, 16)
case("trunc_riff_claims_more",
     riff(fmt_pcm(1, 44100, 16), chunk(b"data", encode(_v, 16, 16)), size_adjust=10),
     "Truncated", (44100, 1, 16), _v, 1, 16)
case("trunc_empty_file", b"", "Truncated")

# ---- 合法但不支持 ----
_d = chunk(b"data", b"\x00" * 8)
case("unsup_8bit", riff(fmt_pcm(2, 44100, 8), _d), "Unsupported")
case("unsup_float_tag3", riff(fmt_pcm(2, 44100, 32, tag=3), _d), "Unsupported")
case("unsup_ext_float", riff(fmt_ext(2, 44100, 32, 32, 3, sub=SUBTYPE_FLOAT), _d), "Unsupported")
case("unsup_3ch", riff(fmt_pcm(3, 44100, 16), chunk(b"data", b"\x00" * 6)), "Unsupported")
case("unsup_stereo_side_mask", riff(fmt_ext(2, 44100, 16, 16, 0x600), _d), "Unsupported")
case("unsup_mono_left_mask", riff(fmt_ext(1, 44100, 16, 16, 0x1), _d), "Unsupported")
case("unsup_rifx", riff(fmt_pcm(2, 44100, 16), _d, magic=b"RIFX"), "Unsupported")
case("unsup_rf64", riff(fmt_pcm(2, 44100, 16), _d, magic=b"RF64"), "Unsupported")
case("unsup_bits64", riff(fmt_pcm(1, 44100, 64), _d), "Unsupported")
case("unsup_rate_384k", riff(fmt_pcm(2, 384000, 16), _d), "Unsupported")
case("unsup_pcm_bits12", riff(fmt_pcm(1, 44100, 12, align=2), _d), "Unsupported")
case("unsup_adpcm_tag2", riff(fmt_pcm(1, 44100, 16, tag=2), _d), "Unsupported")

# ---- 非法 ----
case("mal_block_align", riff(fmt_pcm(2, 44100, 16, align=6), chunk(b"data", b"\x00" * 12)), "Malformed")
case("mal_valid_gt_container", riff(fmt_ext(2, 44100, 16, 20, 3), _d), "Malformed")
case("mal_valid_zero", riff(fmt_ext(2, 44100, 16, 0, 3), _d), "Malformed")
case("mal_data_before_fmt", riff(_d, fmt_pcm(2, 44100, 16)), "Malformed")
case("mal_chunk_beyond_riff", riff(fmt_pcm(2, 44100, 16), _d, size_adjust=-4), "Malformed")
case("mal_data_not_frame_multiple", riff(fmt_pcm(2, 44100, 16), chunk(b"data", b"\x00" * 6)), "Malformed")
case("mal_cbsize_small", riff(fmt_ext(2, 44100, 16, 16, 3, cb=20), _d), "Malformed")
case("mal_channels0", riff(fmt_pcm(0, 44100, 16, align=0), _d), "Malformed")
case("mal_rate0", riff(fmt_pcm(2, 0, 16), _d), "Malformed")
case("mal_not_wave", riff(fmt_pcm(2, 44100, 16), _d, form=b"AVI "), "Malformed")
case("mal_not_riff", riff(fmt_pcm(2, 44100, 16), _d, magic=b"FORM"), "Malformed")
case("mal_dup_fmt", riff(fmt_pcm(2, 44100, 16), fmt_pcm(2, 44100, 16), _d), "Malformed")
case("mal_dup_data", riff(fmt_pcm(2, 44100, 16), _d, _d), "Malformed",
     (44100, 2, 16), [0] * 4, 2, 16)
case("mal_no_data", riff(fmt_pcm(2, 44100, 16)), "Malformed")
case("mal_fmt_short", riff(chunk(b"fmt ", b"\x01\x00" * 7), _d), "Malformed")
case("mal_ext_fmt_short", riff(chunk(b"fmt ", fmt_ext(2, 44100, 16, 16, 3)[8:8 + 30]), _d), "Malformed")
case("mal_mask_too_many_bits", riff(fmt_ext(1, 44100, 16, 16, 0x3), _d), "Malformed")
case("mal_ext_container_not_byte", riff(fmt_ext(1, 44100, 20, 20, 4, align=3), _d), "Malformed")
case("mal_riff_size_lt4", b"RIFF\x02\x00\x00\x00WAVE", "Malformed")
case("mal_tail_lt8_in_riff", riff(fmt_pcm(2, 44100, 16), _d, b"\x00\x00\x00"), "Malformed",
     (44100, 2, 16), [0] * 4, 2, 16)


def ffmpeg_ref(path, ch, c):
    # 显式指定输入解码器：ffmpeg 6.1 会把 C32/V24 的 EXTENSIBLE PCM 误识别为 pcm_f24le。
    out = subprocess.run(["ffmpeg", "-v", "error", "-c:a", f"pcm_s{c}le", "-i", path, "-f", "s32le", "-c:a", "pcm_s32le",
                          "-ac", str(ch), "-"], check=True, capture_output=True).stdout
    return [x[0] for x in struct.iter_unpack("<i", out)]


def main():
    os.makedirs(OUT, exist_ok=True)
    names = set()
    for c in CASES:
        assert c["name"] not in names, c["name"]
        names.add(c["name"])
        wav = os.path.join(OUT, c["name"] + ".wav")
        with open(wav, "wb") as f:
            f.write(c["data"])
        with open(os.path.join(OUT, c["name"] + ".expect"), "w") as f:
            f.write(f"result {c['result']}\n")
            f.write("format " + (" ".join(map(str, c["fmt"])) if c["fmt"] else "none") + "\n")
            f.write("mask " + ("none" if c["mask"] is None else str(c["mask"])) + "\n")
            f.write(f"samples {len(c['vals'])}\n")
            ch = c["ch"]
            for i in range(0, len(c["vals"]), ch):
                f.write(" ".join(map(str, c["vals"][i:i + ch])) + "\n")
        if c["ffmpeg"]:
            ref = ffmpeg_ref(wav, c["ch"], c["c"])
            want = [s << (32 - c["v"]) for s in c["vals"]]
            want = [((x + (1 << 31)) & 0xFFFFFFFF) - (1 << 31) for x in want]
            assert ref == want, f"ffmpeg 参考与生成样本不一致: {c['name']}"
            with open(os.path.join(OUT, c["name"] + ".ffmpeg.s32le"), "wb") as f:
                f.write(struct.pack(f"<{len(ref)}i", *ref))
    print(f"{len(CASES)} cases -> {OUT}")


if __name__ == "__main__":
    main()
