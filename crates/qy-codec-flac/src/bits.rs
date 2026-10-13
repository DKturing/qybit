//! MSB 优先的位读取器。越过输入末尾返回 [`Incomplete`]，不 panic。

/// 输入不足以完成本次读取。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Incomplete;

pub struct Bits<'a> {
    data: &'a [u8],
    /// 已读位数。
    pos: usize,
}

impl<'a> Bits<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn bit_pos(&self) -> usize {
        self.pos
    }

    /// 已读完的整字节数（须字节对齐时调用）。
    pub fn byte_pos(&self) -> usize {
        self.pos / 8
    }

    fn remaining(&self) -> usize {
        self.data.len() * 8 - self.pos
    }

    /// 读取 `n ≤ 32` 位无符号数。
    pub fn read(&mut self, n: u32) -> Result<u32, Incomplete> {
        if n == 0 {
            return Ok(0);
        }
        let n_us = n as usize;
        if n_us > self.remaining() {
            self.pos = self.data.len() * 8;
            return Err(Incomplete);
        }
        let byte = self.pos / 8;
        let off = (self.pos % 8) as u32;
        let mut v: u64 = 0;
        for i in 0..5 {
            v = (v << 8) | u64::from(self.data.get(byte + i).copied().unwrap_or(0));
        }
        // v 含 40 位；off + n ≤ 7 + 32 = 39。
        let shifted = v >> (40 - off - n);
        let mask = if n == 32 {
            u64::from(u32::MAX)
        } else {
            (1u64 << n) - 1
        };
        self.pos += n_us;
        Ok((shifted & mask) as u32)
    }

    /// 读取 `n ≤ 33` 位二进制补码有符号数。
    pub fn read_signed(&mut self, n: u32) -> Result<i64, Incomplete> {
        match n {
            0 => Ok(0),
            33 => {
                let hi = self.read(1)?;
                let lo = self.read(32)?;
                let v = (u64::from(hi) << 32) | u64::from(lo);
                Ok(((v << 31) as i64) >> 31)
            }
            _ => {
                let v = u64::from(self.read(n)?);
                let sh = 64 - n;
                Ok(((v << sh) as i64) >> sh)
            }
        }
    }

    /// 一元码：连续 0 的个数，直到读到 1（1 被消耗）。
    pub fn unary(&mut self) -> Result<u64, Incomplete> {
        let mut count = 0u64;
        // 先处理当前字节剩余部分，再按整字节扫描。
        loop {
            let byte = self.pos / 8;
            let Some(&b) = self.data.get(byte) else {
                return Err(Incomplete);
            };
            let off = (self.pos % 8) as u32;
            let rest = (b << off) as u32 & 0xFF; // 剩余位置于高位
            let avail = 8 - off;
            if rest == 0 {
                count += u64::from(avail);
                self.pos += avail as usize;
                continue;
            }
            let lz = rest.leading_zeros() - 24; // 在 8 位内的前导零
            count += u64::from(lz);
            self.pos += lz as usize + 1;
            return Ok(count);
        }
    }

    /// 跳到下一个字节边界，返回被跳过的位（须全部为 0 才合法，由调用方检查）。
    pub fn align(&mut self) -> Result<u32, Incomplete> {
        let pad = ((8 - self.pos % 8) % 8) as u32;
        self.read(pad)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_bits_signed_and_unary() {
        let d = [0b1010_1100, 0b0000_0001, 0xFF, 0x80, 0x00, 0x00, 0x00, 0x01];
        let mut b = Bits::new(&d);
        assert_eq!(b.read(3), Ok(0b101));
        assert_eq!(b.read_signed(5), Ok(0b01100_i64)); // 12
        assert_eq!(b.unary(), Ok(7)); // 0000000 1
        assert_eq!(b.read_signed(8), Ok(-1));
        assert_eq!(b.read(1), Ok(1));
        assert_eq!(b.unary(), Ok(38)); // 0x80 余 7 个 0 + 3 个零字节 + 0x01 的 7 个 0
        assert_eq!(b.bit_pos(), 64);
        assert_eq!(b.read(1), Err(Incomplete));
        let d = [0x80, 0, 0, 0, 0x00];
        let mut b = Bits::new(&d);
        assert_eq!(b.read_signed(33), Ok(-(1i64 << 32)));
        let d = [0x7F, 0xFF, 0xFF, 0xFF, 0x80];
        let mut b = Bits::new(&d);
        assert_eq!(b.read_signed(33), Ok((1i64 << 32) - 1));
        let mut b = Bits::new(&[0u8, 0]);
        assert_eq!(b.unary(), Err(Incomplete));
    }
}
