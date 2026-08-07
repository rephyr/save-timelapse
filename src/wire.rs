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
