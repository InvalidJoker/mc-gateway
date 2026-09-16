use crate::{Error, Result};

/// Maximum encoded size of a 32-bit VarInt.
pub const MAX_VARINT_LEN: usize = 5;

/// Reads a VarInt starting at `*pos`, advancing `pos` past it.
///
/// Returns [`Error::Incomplete`] when the buffer ends mid-VarInt, so a caller
/// streaming from a socket can simply read more and try again.
pub fn read_varint(buf: &[u8], pos: &mut usize) -> Result<i32> {
    let mut value: u32 = 0;
    let mut shift = 0u32;
    let mut cursor = *pos;

    loop {
        let byte = *buf.get(cursor).ok_or(Error::Incomplete)?;
        cursor += 1;
        value |= u32::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift >= 32 {
            return Err(Error::VarIntTooLong);
        }
    }

    *pos = cursor;
    Ok(value as i32)
}

/// Appends the VarInt encoding of `value` to `out`.
pub fn write_varint(out: &mut Vec<u8>, value: i32) {
    let mut value = value as u32;
    loop {
        if value & !0x7F == 0 {
            out.push(value as u8);
            return;
        }
        out.push((value as u8 & 0x7F) | 0x80);
        value >>= 7;
    }
}

/// Number of bytes [`write_varint`] would produce.
pub const fn varint_len(value: i32) -> usize {
    let value = value as u32;
    match value {
        0..=0x7F => 1,
        0x80..=0x3FFF => 2,
        0x4000..=0x1F_FFFF => 3,
        0x20_0000..=0xFFF_FFFF => 4,
        _ => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CASES: &[(i32, &[u8])] = &[
        (0, &[0x00]),
        (1, &[0x01]),
        (2, &[0x02]),
        (127, &[0x7f]),
        (128, &[0x80, 0x01]),
        (255, &[0xff, 0x01]),
        (25565, &[0xdd, 0xc7, 0x01]),
        (2097151, &[0xff, 0xff, 0x7f]),
        (2147483647, &[0xff, 0xff, 0xff, 0xff, 0x07]),
        (-1, &[0xff, 0xff, 0xff, 0xff, 0x0f]),
        (-2147483648, &[0x80, 0x80, 0x80, 0x80, 0x08]),
    ];

    #[test]
    fn matches_protocol_reference_vectors() {
        for (value, encoded) in CASES {
            let mut out = Vec::new();
            write_varint(&mut out, *value);
            assert_eq!(&out, encoded, "encoding {value}");
            assert_eq!(varint_len(*value), encoded.len(), "length of {value}");

            let mut pos = 0;
            assert_eq!(read_varint(encoded, &mut pos).unwrap(), *value);
            assert_eq!(pos, encoded.len());
        }
    }

    #[test]
    fn truncated_varint_is_incomplete_not_an_error() {
        let mut pos = 0;
        assert_eq!(read_varint(&[0xdd, 0xc7], &mut pos), Err(Error::Incomplete));
        let mut pos = 0;
        assert_eq!(read_varint(&[], &mut pos), Err(Error::Incomplete));
    }

    #[test]
    fn rejects_oversized_varint() {
        let mut pos = 0;
        let bomb = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff];
        assert_eq!(read_varint(&bomb, &mut pos), Err(Error::VarIntTooLong));
    }

    #[test]
    fn roundtrips_across_the_whole_range() {
        for value in (i32::MIN..=i32::MAX).step_by(0x10_0001) {
            let mut out = Vec::new();
            write_varint(&mut out, value);
            let mut pos = 0;
            assert_eq!(read_varint(&out, &mut pos).unwrap(), value);
            assert_eq!(pos, out.len());
        }
    }
}
