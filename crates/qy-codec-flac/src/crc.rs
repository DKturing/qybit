//! FLAC 帧头 CRC-8 与帧 CRC-16（RFC 9639 §9.1.8、§9.3）。
//!
//! CRC-8：多项式 x^8 + x^2 + x + 1（0x07），初值 0，不反射；
//! CRC-16：多项式 x^16 + x^15 + x^2 + 1（0x8005），初值 0，不反射。

const fn table8() -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u8;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x80 != 0 {
                (c << 1) ^ 0x07
            } else {
                c << 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

const fn table16() -> [u16; 256] {
    let mut t = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u16) << 8;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x8000 != 0 {
                (c << 1) ^ 0x8005
            } else {
                c << 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static T8: [u8; 256] = table8();
static T16: [u16; 256] = table16();

pub fn crc8(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |c, &b| T8[usize::from(c ^ b)])
}

pub fn crc16(data: &[u8]) -> u16 {
    data.iter().fold(0u16, |c, &b| {
        (c << 8) ^ T16[usize::from((c >> 8) as u8 ^ b)]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        // CRC-8/SMBUS 与 CRC-16/BUYPASS(UMTS) 的标准校验值（"123456789"）。
        assert_eq!(crc8(b"123456789"), 0xF4);
        assert_eq!(crc16(b"123456789"), 0xFEE8);
        assert_eq!(crc8(&[]), 0);
        assert_eq!(crc16(&[]), 0);
    }
}
