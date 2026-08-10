//! Little endian byte reader and writer shared by the binary wire formats in
//! this project: frame files (see `frame.rs`) and live capture event
//! segments (see `event.rs`). Both are written by `mod/control.lua`, whose
//! Lua runtime has no `string.pack`, so the formats stick to fixed width
//! integers and length prefixed strings rather than anything a packer would
//! normally reach for.
//!
//! `settings_dat.rs` already has a private cursor of this shape for
//! `mod-settings.dat`. This is a separate, slightly more general version
//! rather than a shared one, since that file's cursor returns `io::Result`
//! for a format that is always read whole, while the formats here need the
//! softer "not enough bytes left" signal a reader can choose to treat as
//! either a hard error (a frame file) or a normal end of stream (an event
//! segment whose last record was cut off by a killed process).

pub struct ByteReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> ByteReader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        ByteReader { bytes, pos: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    /// How many bytes have been read so far. A caller re-slicing from a
    /// fixed starting offset on every call (rather than holding one
    /// `ByteReader` across an owned buffer it also wants to mutate) uses
    /// this to know how far to advance that offset.
    pub fn consumed(&self) -> usize {
        self.pos
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let slice = self.bytes.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    /// Steps over `n` bytes without interpreting them.
    ///
    /// This is what makes an extension record skippable: a reader that does
    /// not recognise a tag still knows how many bytes the record occupies, so
    /// it can resume at the next one instead of desynchronising. `None` means
    /// the record claimed more bytes than the file has left, which is a
    /// truncated file rather than merely an unfamiliar feature.
    pub fn skip(&mut self, n: usize) -> Option<()> {
        self.take(n).map(|_| ())
    }

    pub fn magic(&mut self, expected: &[u8; 4]) -> Option<()> {
        let got = self.take(4)?;
        (got == expected).then_some(())
    }

    pub fn tag(&mut self) -> Option<u8> {
        self.u8()
    }

    pub fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|s| s[0])
    }

    pub fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|s| u16::from_le_bytes(s.try_into().unwrap()))
    }

    pub fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|s| u32::from_le_bytes(s.try_into().unwrap()))
    }

    pub fn u64(&mut self) -> Option<u64> {
        self.take(8).map(|s| u64::from_le_bytes(s.try_into().unwrap()))
    }

    /// LEB128: seven bits per byte, high bit set while more follow.
    ///
    /// Refuses anything longer than ten bytes, which is the most a `u64` can
    /// legitimately need, so a corrupt stream of all-high-bit bytes ends the
    /// read rather than looping to the end of the file.
    pub fn varint(&mut self) -> Option<u64> {
        let mut value = 0u64;
        for shift in (0..70).step_by(7) {
            let byte = self.u8()?;
            if shift == 63 && byte > 1 {
                return None; // would overflow a u64
            }
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    /// Zigzag: small magnitudes stay small whichever side of zero they are
    /// on, so a coordinate delta of -1 costs one byte rather than ten.
    pub fn varint_i32(&mut self) -> Option<i32> {
        let raw = self.varint()?;
        Some(((raw >> 1) as i64 ^ -((raw & 1) as i64)) as i32)
    }

    pub fn i32(&mut self) -> Option<i32> {
        self.take(4).map(|s| i32::from_le_bytes(s.try_into().unwrap()))
    }

    /// A `u16` length prefix followed by that many UTF-8 bytes. Prototype and
    /// surface names are always short, so a `u16` length leaves plenty of
    /// headroom without spending 4 bytes on every single one.
    pub fn string(&mut self) -> Option<String> {
        let len = self.u16()? as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).ok()
    }
}

#[derive(Default)]
pub struct ByteWriter {
    buf: Vec<u8>,
}

impl ByteWriter {
    pub fn new() -> Self {
        ByteWriter::default()
    }

    pub fn magic(&mut self, value: &[u8; 4]) -> &mut Self {
        self.buf.extend_from_slice(value);
        self
    }

    pub fn u8(&mut self, value: u8) -> &mut Self {
        self.buf.push(value);
        self
    }

    pub fn u16(&mut self, value: u16) -> &mut Self {
        self.buf.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub fn u32(&mut self, value: u32) -> &mut Self {
        self.buf.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub fn u64(&mut self, value: u64) -> &mut Self {
        self.buf.extend_from_slice(&value.to_le_bytes());
        self
    }

    /// See `ByteReader::varint`.
    pub fn varint(&mut self, mut value: u64) -> &mut Self {
        while value >= 0x80 {
            self.buf.push((value as u8) | 0x80);
            value >>= 7;
        }
        self.buf.push(value as u8);
        self
    }

    /// See `ByteReader::varint_i32`.
    pub fn varint_i32(&mut self, value: i32) -> &mut Self {
        self.varint(((value << 1) ^ (value >> 31)) as u32 as u64)
    }

    pub fn i32(&mut self, value: i32) -> &mut Self {
        self.buf.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub fn string(&mut self, value: &str) -> &mut Self {
        self.u16(value.len() as u16);
        self.buf.extend_from_slice(value.as_bytes());
        self
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip_across_the_whole_range() {
        let mut w = ByteWriter::new();
        let values = [0u64, 1, 127, 128, 300, 16_383, 16_384, u32::MAX as u64, u64::MAX];
        for v in values {
            w.varint(v);
        }
        let bytes = w.into_vec();
        let mut r = ByteReader::new(&bytes);
        for v in values {
            assert_eq!(r.varint(), Some(v), "round trip {v}");
        }
    }

    /// The property the frame format leans on: a small value costs one byte,
    /// which is what makes coordinate deltas cheap.
    #[test]
    fn small_varints_cost_one_byte_and_grow_seven_bits_at_a_time() {
        let len = |v: u64| {
            let mut w = ByteWriter::new();
            w.varint(v);
            w.into_vec().len()
        };
        assert_eq!(len(0), 1);
        assert_eq!(len(127), 1);
        assert_eq!(len(128), 2);
        assert_eq!(len(16_383), 2);
        assert_eq!(len(16_384), 3);
        assert_eq!(len(u64::MAX), 10);
    }

    /// Zigzag is why a delta of -1 is as cheap as +1: without it a negative
    /// would set every high bit and take the full ten bytes.
    #[test]
    fn zigzag_keeps_small_negatives_small() {
        let len = |v: i32| {
            let mut w = ByteWriter::new();
            w.varint_i32(v);
            w.into_vec().len()
        };
        assert_eq!(len(0), 1);
        assert_eq!(len(-1), 1);
        assert_eq!(len(63), 1);
        assert_eq!(len(-64), 1);
        assert_eq!(len(-65), 2);

        let mut w = ByteWriter::new();
        let values = [0i32, 1, -1, 63, -64, 1000, -1000, i32::MAX, i32::MIN];
        for v in values {
            w.varint_i32(v);
        }
        let bytes = w.into_vec();
        let mut r = ByteReader::new(&bytes);
        for v in values {
            assert_eq!(r.varint_i32(), Some(v), "round trip {v}");
        }
    }

    /// A stream of all-high-bit bytes must end the read rather than loop to
    /// the end of the file looking for a terminator that never comes.
    #[test]
    fn a_corrupt_unterminated_varint_is_none_rather_than_a_runaway() {
        let bytes = [0xffu8; 32];
        assert_eq!(ByteReader::new(&bytes).varint(), None);
    }

    #[test]
    fn a_varint_too_large_for_a_u64_is_rejected() {
        // Ten bytes whose final byte pushes past 64 bits.
        let bytes = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f];
        assert_eq!(ByteReader::new(&bytes).varint(), None);
    }

    #[test]
    fn primitives_round_trip() {
        let mut w = ByteWriter::new();
        w.magic(b"TEST").u8(200).u16(40000).u32(3_000_000_000).u64(10_000_000_000_000).i32(-805).string("nauvis");
        let bytes = w.into_vec();

        let mut r = ByteReader::new(&bytes);
        assert_eq!(r.magic(b"TEST"), Some(()));
        assert_eq!(r.u8(), Some(200));
        assert_eq!(r.u16(), Some(40000));
        assert_eq!(r.u32(), Some(3_000_000_000));
        assert_eq!(r.u64(), Some(10_000_000_000_000));
        assert_eq!(r.i32(), Some(-805));
        assert_eq!(r.string().as_deref(), Some("nauvis"));
        assert!(r.is_empty());
    }

    #[test]
    fn a_mismatched_magic_is_none_not_a_panic() {
        let mut r = ByteReader::new(b"NOPE");
        assert_eq!(r.magic(b"TEST"), None);
    }

    #[test]
    fn reading_past_the_end_is_none_rather_than_panicking() {
        let mut r = ByteReader::new(&[1, 2]);
        assert_eq!(r.u8(), Some(1));
        assert_eq!(r.u32(), None, "only one byte left of the four needed");
    }
}
