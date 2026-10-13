//! 静态帧队列测试：与按字节建模的参考队列逐字节比对（随机入队/出队/部分提交/换帧宽）。

use std::collections::VecDeque;
use std::vec;
use std::vec::Vec;

use super::*;
use crate::{Endian, Placement, SourceFormat};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + (self.next() % (hi - lo + 1) as u64) as usize
    }
}

/// (声道, 源位数, 容器, 有效位)：覆盖帧宽 2/3/4/6/8 字节。
const FORMATS: [(u16, u8, u8, u8); 6] = [
    (1, 16, 16, 16),
    (1, 24, 24, 24),
    (2, 16, 16, 16),
    (2, 24, 24, 24),
    (2, 20, 32, 24),
    (2, 32, 32, 32),
];

fn formats(i: usize) -> (SourceFormat, DeviceFormat) {
    let (ch, n, c, v) = FORMATS[i];
    let (Ok(s), Ok(d)) = (
        SourceFormat::new(8_000, ch, n),
        DeviceFormat::new(8_000, ch, c, v, Endian::Little, Placement::MsbAligned),
    ) else {
        panic!("格式");
    };
    (s, d)
}

/// 生成 `frames` 帧可辨识的样本（各帧不同，限定在 N 位范围内）。
fn samples(src: &SourceFormat, start: usize, frames: usize) -> Vec<i32> {
    let ch = usize::from(src.channels());
    let bits = u32::from(src.bits());
    let span = 1i64 << bits;
    (0..frames * ch)
        .map(|i| {
            let x = ((start * ch + i) as i64).wrapping_mul(2_654_435_761) % span;
            (x - span / 2) as i32
        })
        .collect()
}

fn packed(src: &SourceFormat, dev: &DeviceFormat, s: &[i32]) -> Vec<u8> {
    let Ok(pcm) = Pcm::exact(s, *src) else {
        panic!("pcm");
    };
    let mut out = vec![0u8; pcm.frames() * usize::from(dev.block_align())];
    assert_eq!(pack(&pcm, dev, &mut out), Ok(pcm.frames()));
    out
}

const CAP: usize = 1_000;
static RANDOM: StaticFrameQueue<CAP> = StaticFrameQueue::new();

#[test]
fn random_ops_match_byte_model() {
    let Some((mut p, mut c)) = RANDOM.split() else {
        panic!("split");
    };
    assert!(RANDOM.split().is_none(), "只能取得一次两端");
    let mut rng = Rng(0x5157_4954);
    let mut model: VecDeque<u8> = VecDeque::new();
    let mut generation = 0u64;
    let (mut wraps_full, mut partial) = (0, 0);
    let mut out = vec![0u8; CAP];
    for round in 0..200 {
        let k = rng.range(0, FORMATS.len() - 1);
        let (src, dev) = formats(k);
        let fb = usize::from(dev.block_align());
        generation += 1;
        assert_eq!(reconfigure(&mut p, &mut c, &dev, 0, generation), Ok(()));
        model.clear();
        assert_eq!(c.available_frames(), 0);
        assert_eq!(p.queued_frames(), 0);
        let mut next = 0usize;
        for _ in 0..rng.range(20, 200) {
            if rng.range(0, 1) == 0 {
                let frames = rng.range(1, 3 * CAP / fb);
                let s = samples(&src, next, frames);
                let Ok(pcm) = Pcm::exact(&s, src) else {
                    panic!("pcm");
                };
                let Ok(n) = p.pack_push(&pcm, &dev) else {
                    panic!("push");
                };
                let bytes = packed(&src, &dev, &s[..n * usize::from(src.channels())]);
                model.extend(bytes);
                next += n;
                if n == 0 {
                    // 写不进时，尾部与头部的空闲段都不足一帧。
                    assert!(
                        model.len() + 2 * fb > CAP,
                        "过早报满 {} / {CAP}",
                        model.len()
                    );
                    wraps_full += 1;
                }
            } else if rng.range(0, 1) == 0 {
                let want = rng.range(0, CAP / fb);
                let Ok(n) = c.pop_frames(&mut out[..want * fb]) else {
                    panic!("pop");
                };
                let expect: Vec<u8> = model.drain(..n * fb).collect();
                assert_eq!(&out[..n * fb], &expect[..], "第 {round} 轮出队字节不一致");
                assert!(n == want || model.is_empty(), "可取未取");
            } else {
                let max = rng.range(1, CAP / fb);
                let take = rng.range(0, max);
                let mut seen = Vec::new();
                let Ok(n) = c.consume_contiguous(max, |b| {
                    assert_eq!(b.len() % fb, 0, "交出的不是整帧");
                    seen.extend_from_slice(b);
                    take.min(b.len() / fb)
                }) else {
                    panic!("consume");
                };
                let head: Vec<u8> = model.iter().take(seen.len()).copied().collect();
                assert_eq!(seen, head, "连续段内容不一致");
                if n * fb < seen.len() {
                    partial += 1;
                }
                model.drain(..n * fb);
            }
            assert_eq!(c.available_frames() * fb, model.len());
            assert_eq!(p.queued_frames() * fb, model.len());
        }
    }
    assert!(wraps_full > 0 && partial > 0, "未覆盖满队列/部分提交");
}

static A: StaticFrameQueue<240> = StaticFrameQueue::new();
static B: StaticFrameQueue<240> = StaticFrameQueue::new();

#[test]
fn reconfigure_checks() {
    let (Some((mut pa, mut ca)), Some((mut pb, mut cb))) = (A.split(), B.split()) else {
        panic!("split");
    };
    let (src, dev) = formats(5);
    let Ok(pcm) = Pcm::exact(&[1, 2], src) else {
        panic!("pcm");
    };
    assert_eq!(pa.pack_push(&pcm, &dev), Err(RingError::NotConfigured));
    assert_eq!(
        reconfigure(&mut pa, &mut cb, &dev, 0, 1),
        Err(RingError::Mismatched)
    );
    // 保证量 = 240 − 2×8。
    assert_eq!(
        reconfigure(&mut pa, &mut ca, &dev, 225, 1),
        Err(RingError::CapacityExceeded {
            required: 225,
            capacity: 224
        })
    );
    assert_eq!(reconfigure(&mut pa, &mut ca, &dev, 224, 1), Ok(()));
    assert_eq!(
        reconfigure(&mut pa, &mut ca, &dev, 0, 1),
        Err(RingError::StaleGeneration)
    );
    let (_, mono) = formats(0);
    assert_eq!(pa.pack_push(&pcm, &mono), Err(RingError::FrameWidth));
    assert_eq!(pa.pack_push(&pcm, &dev), Ok(1));
    assert_eq!(FrameSource::available_frames(&ca), 1);
    // 换代次丢弃旧数据。
    assert_eq!(reconfigure(&mut pa, &mut ca, &dev, 0, 2), Ok(()));
    assert_eq!(FrameSource::available_frames(&ca), 0);
    assert_eq!(ca.pop_frames(&mut [0u8; 8]), Ok(0));
    assert_eq!(reconfigure(&mut pb, &mut cb, &dev, 0, 1), Ok(()));
}
