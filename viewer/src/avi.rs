//! Writing Motion JPEG video into an AVI container, by hand.
//!
//! The point of doing this rather than reaching for an encoder library is the
//! promise on the tin: you run the executable and that is it, nothing to
//! install. AVI is old and simple enough that writing it is a few hundred
//! lines of laying out headers, and MJPEG is just "every frame is a complete
//! JPEG", so there is no inter-frame prediction, no bitrate control and no
//! state to get wrong. Every mainstream player and browser reads it.
//!
//! The cost is size: with no prediction between frames, an MJPEG file is
//! roughly the sum of its frames. That is a real trade, and it is the right
//! one here only because a timelapse is flat colour on a dark background,
//! which JPEG compresses hard.
//!
//! ## Layout
//!
//! ```text
//! RIFF <size> AVI
//!   LIST <size> hdrl
//!     avih <56>            main header: frame rate, count, dimensions
//!     LIST <size> strl
//!       strh <56>          stream header: 'vids' / 'MJPG', rate, length
//!       strf <40>          BITMAPINFOHEADER, biCompression = 'MJPG'
//!   LIST <size> movi
//!     00dc <size> <jpeg>   one per frame, padded to an even length
//!   idx1 <size>            16 bytes per frame: id, flags, offset, length
//! ```
//!
//! Four fields cannot be known until the last frame has been written (the
//! RIFF size, the movi list size, and the frame count in two separate
//! headers), so they are written as zero and patched by seeking back in
//! [`AviWriter::finish`]. That is why this takes a path rather than a writer:
//! it needs to seek.

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

/// Says the file carries an `idx1` index. Players use it to seek; without it
/// many will still play the file but scrubbing degrades to scanning.
const AVIF_HASINDEX: u32 = 0x0000_0010;
/// Every MJPEG frame is a complete image, so every frame is a keyframe.
const AVIIF_KEYFRAME: u32 = 0x0000_0010;

const AVIH_SIZE: u32 = 56;
const STRH_SIZE: u32 = 56;
const STRF_SIZE: u32 = 40;

pub struct AviWriter {
    file: File,
    /// `(offset from the movi fourcc, byte length)` per frame, for `idx1`.
    index: Vec<(u32, u32)>,
    /// Where the `movi` fourcc sits, which index offsets are relative to.
    movi_fourcc_pos: u64,
    /// Positions of the four fields patched in `finish`.
    riff_size_pos: u64,
    movi_size_pos: u64,
    avih_frames_pos: u64,
    strh_length_pos: u64,
    /// Both header copies of `dwSuggestedBufferSize`, which is the largest
    /// frame in the file and therefore also unknown until the end.
    avih_buffer_pos: u64,
    strh_buffer_pos: u64,
    largest_frame: u32,
}

fn u32_at(file: &mut File, pos: u64, value: u32) -> io::Result<()> {
    file.seek(SeekFrom::Start(pos))?;
    file.write_all(&value.to_le_bytes())
}

impl AviWriter {
    pub fn create(path: &Path, width: u32, height: u32, fps: u32) -> io::Result<AviWriter> {
        let mut file = File::create(path)?;
        // Zero would divide by zero below, and something absurd would make a
        // file no player will touch.
        let fps = fps.clamp(1, 240);

        file.write_all(b"RIFF")?;
        let riff_size_pos = file.stream_position()?;
        file.write_all(&0u32.to_le_bytes())?; // patched in finish
        file.write_all(b"AVI ")?;

        // hdrl: everything a player needs before the first frame.
        file.write_all(b"LIST")?;
        let hdrl_size = 4 + (8 + AVIH_SIZE) + (12 + (8 + STRH_SIZE) + (8 + STRF_SIZE));
        file.write_all(&hdrl_size.to_le_bytes())?;
        file.write_all(b"hdrl")?;

        file.write_all(b"avih")?;
        file.write_all(&AVIH_SIZE.to_le_bytes())?;
        file.write_all(&(1_000_000 / fps).to_le_bytes())?; // microseconds per frame
        file.write_all(&0u32.to_le_bytes())?; // max bytes per second, advisory only
        file.write_all(&0u32.to_le_bytes())?; // padding granularity
        file.write_all(&AVIF_HASINDEX.to_le_bytes())?;
        let avih_frames_pos = file.stream_position()?;
        file.write_all(&0u32.to_le_bytes())?; // total frames, patched in finish
        file.write_all(&0u32.to_le_bytes())?; // initial frames
        file.write_all(&1u32.to_le_bytes())?; // one stream: video, no audio
        let avih_buffer_pos = file.stream_position()?;
        file.write_all(&0u32.to_le_bytes())?; // suggested buffer size, patched in finish
        file.write_all(&width.to_le_bytes())?;
        file.write_all(&height.to_le_bytes())?;
        file.write_all(&[0u8; 16])?; // reserved

        file.write_all(b"LIST")?;
        file.write_all(&(4u32 + (8 + STRH_SIZE) + (8 + STRF_SIZE)).to_le_bytes())?;
        file.write_all(b"strl")?;

        file.write_all(b"strh")?;
        file.write_all(&STRH_SIZE.to_le_bytes())?;
        file.write_all(b"vids")?;
        file.write_all(b"MJPG")?;
        file.write_all(&0u32.to_le_bytes())?; // flags
        file.write_all(&0u16.to_le_bytes())?; // priority
        file.write_all(&0u16.to_le_bytes())?; // language
        file.write_all(&0u32.to_le_bytes())?; // initial frames
                                              // Rate over scale is the frame rate as a fraction, so an integer fps
                                              // is exact rather than rounded into a float somewhere.
        file.write_all(&1u32.to_le_bytes())?; // scale
        file.write_all(&fps.to_le_bytes())?; // rate
        file.write_all(&0u32.to_le_bytes())?; // start
        let strh_length_pos = file.stream_position()?;
        file.write_all(&0u32.to_le_bytes())?; // length in frames, patched in finish
        let strh_buffer_pos = file.stream_position()?;
        file.write_all(&0u32.to_le_bytes())?; // suggested buffer size, patched in finish
        file.write_all(&u32::MAX.to_le_bytes())?; // quality: -1 means default
        file.write_all(&0u32.to_le_bytes())?; // sample size: 0 for video
        for value in [0i16, 0, width as i16, height as i16] {
            file.write_all(&value.to_le_bytes())?;
        }

        file.write_all(b"strf")?;
        file.write_all(&STRF_SIZE.to_le_bytes())?;
        file.write_all(&STRF_SIZE.to_le_bytes())?; // biSize
        file.write_all(&(width as i32).to_le_bytes())?;
        // Negative biHeight, and this is the field that decides which way up
        // the video plays. A BITMAPINFOHEADER's height carries the row order
        // in its sign: positive means bottom-up, which is the DIB convention
        // and the one everybody writes by accident, and negative means
        // top-down. Our rows come off the render target top-down, so a
        // positive value here is a lie about the data, and a player that
        // believes it renders the whole video upside down. Declaring the
        // truth is also the more compatible of the two fixes: a player that
        // ignores the sign treats MJPEG as top-down anyway, which is what
        // these frames already are, so both kinds of player agree.
        file.write_all(&(-(height as i32)).to_le_bytes())?;
        file.write_all(&1u16.to_le_bytes())?; // planes
        file.write_all(&24u16.to_le_bytes())?; // bits per pixel
        file.write_all(b"MJPG")?; // biCompression
        file.write_all(&(width * height * 3).to_le_bytes())?; // biSizeImage
        file.write_all(&0i32.to_le_bytes())?; // x pixels per meter
        file.write_all(&0i32.to_le_bytes())?; // y pixels per meter
        file.write_all(&0u32.to_le_bytes())?; // colours used
        file.write_all(&0u32.to_le_bytes())?; // colours important

        file.write_all(b"LIST")?;
        let movi_size_pos = file.stream_position()?;
        file.write_all(&0u32.to_le_bytes())?; // patched in finish
        let movi_fourcc_pos = file.stream_position()?;
        file.write_all(b"movi")?;

        Ok(AviWriter {
            file,
            index: Vec::new(),
            movi_fourcc_pos,
            riff_size_pos,
            movi_size_pos,
            avih_frames_pos,
            strh_length_pos,
            avih_buffer_pos,
            strh_buffer_pos,
            largest_frame: 0,
        })
    }

    /// Appends one already-encoded JPEG as the next frame.
    pub fn add_jpeg(&mut self, jpeg: &[u8]) -> io::Result<()> {
        let chunk_pos = self.file.stream_position()?;
        // Relative to the `movi` fourcc, which is the convention every player
        // expects. Getting this wrong does not stop playback (the frames are
        // still in order) but breaks seeking, which is the one thing an index
        // exists for.
        let offset = (chunk_pos - self.movi_fourcc_pos) as u32;
        let length = jpeg.len() as u32;

        self.file.write_all(b"00dc")?;
        self.file.write_all(&length.to_le_bytes())?;
        self.file.write_all(jpeg)?;
        // RIFF chunks are word aligned. Without this a frame of odd length
        // shifts everything after it by one byte and the file stops parsing
        // at that point.
        if length % 2 == 1 {
            self.file.write_all(&[0u8])?;
        }

        self.index.push((offset, length));
        self.largest_frame = self.largest_frame.max(length);
        Ok(())
    }

    pub fn frames(&self) -> usize {
        self.index.len()
    }

    /// Writes the index and patches every size that could not be known while
    /// frames were still arriving.
    pub fn finish(mut self) -> io::Result<()> {
        let movi_end = self.file.stream_position()?;
        // The list covers its own `movi` fourcc plus every chunk after it.
        let movi_size = (movi_end - self.movi_fourcc_pos) as u32;

        self.file.write_all(b"idx1")?;
        self.file.write_all(&(self.index.len() as u32 * 16).to_le_bytes())?;
        for &(offset, length) in &self.index {
            self.file.write_all(b"00dc")?;
            self.file.write_all(&AVIIF_KEYFRAME.to_le_bytes())?;
            self.file.write_all(&offset.to_le_bytes())?;
            self.file.write_all(&length.to_le_bytes())?;
        }

        let end = self.file.stream_position()?;
        let frames = self.index.len() as u32;
        // Everything after the "RIFF" fourcc and its own size field.
        u32_at(&mut self.file, self.riff_size_pos, (end - 8) as u32)?;
        u32_at(&mut self.file, self.movi_size_pos, movi_size)?;
        u32_at(&mut self.file, self.avih_frames_pos, frames)?;
        u32_at(&mut self.file, self.strh_length_pos, frames)?;
        // How large a buffer a player needs to hold one frame. Advisory, but
        // some decoders size their input buffer from it and truncate frames
        // that exceed it, so it has to be the real maximum rather than a
        // guess.
        u32_at(&mut self.file, self.avih_buffer_pos, self.largest_frame)?;
        u32_at(&mut self.file, self.strh_buffer_pos, self.largest_frame)?;

        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-pixel JPEG. Its bytes are never decoded by these tests, which
    /// only care that whatever went in comes back out intact and in order.
    fn fake_jpeg(len: usize) -> Vec<u8> {
        let mut data = vec![0x5A; len];
        data[..2].copy_from_slice(&[0xFF, 0xD8]);
        let n = data.len();
        data[n - 2..].copy_from_slice(&[0xFF, 0xD9]);
        data
    }

    fn u32_at(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    /// Walks the file the way a player does and hands back every frame.
    ///
    /// Deliberately parses rather than checking bytes at fixed offsets: a
    /// player follows chunk sizes, so a size that is wrong by one is exactly
    /// the bug worth catching, and a fixed-offset assertion would sail past
    /// it.
    fn frames_in(bytes: &[u8]) -> Vec<Vec<u8>> {
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"AVI ");

        let mut movi = None;
        let mut at = 12;
        while at + 8 <= bytes.len() {
            let id = &bytes[at..at + 4];
            let size = u32_at(bytes, at + 4) as usize;
            if id == b"LIST" && &bytes[at + 8..at + 12] == b"movi" {
                movi = Some((at + 8, size));
            }
            at += 8 + size + (size & 1);
        }

        let (start, size) = movi.expect("a movi list");
        let mut frames = Vec::new();
        let mut at = start + 4;
        while at + 8 <= start + size {
            let id = &bytes[at..at + 4];
            let length = u32_at(bytes, at + 4) as usize;
            if id == b"00dc" {
                frames.push(bytes[at + 8..at + 8 + length].to_vec());
            }
            at += 8 + length + (length & 1);
        }
        frames
    }

    fn write(frames: &[Vec<u8>]) -> (tempfile::TempDir, Vec<u8>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.avi");
        let mut writer = AviWriter::create(&path, 64, 48, 30).unwrap();
        for frame in frames {
            writer.add_jpeg(frame).unwrap();
        }
        assert_eq!(writer.frames(), frames.len());
        writer.finish().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        (dir, bytes)
    }

    /// Finds the BITMAPINFOHEADER by its chunk id rather than a fixed
    /// offset, so this keeps testing the right field if the header layout
    /// ever moves. `biWidth` and `biHeight` are the two i32s after `biSize`.
    fn strf_dimensions(bytes: &[u8]) -> (i32, i32) {
        let at = bytes.windows(4).position(|w| w == b"strf").expect("strf chunk present") + 8;
        let read = |offset: usize| i32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        (read(at + 4), read(at + 8))
    }

    /// The field that decides which way up the video plays, and the one that
    /// shipped wrong: a positive `biHeight` declares bottom-up rows, and the
    /// frames written here are top-down, so a player that believes the
    /// header renders the entire video upside down. Nothing about the file
    /// looks broken when this is wrong, which is exactly why it needs a test
    /// rather than a careful reading.
    #[test]
    fn the_header_declares_top_down_rows_so_the_video_is_not_upside_down() {
        let (_dir, bytes) = write(&[fake_jpeg(64)]);
        let (width, height) = strf_dimensions(&bytes);
        assert_eq!(width, 64, "width is unsigned in practice and stays positive");
        assert_eq!(height, -48, "negative height means top-down, which is how these frames are written");
    }

    #[test]
    fn every_frame_comes_back_byte_for_byte_and_in_order() {
        let written: Vec<Vec<u8>> = [120, 340, 90].iter().map(|&n| fake_jpeg(n)).collect();
        let (_dir, bytes) = write(&written);
        assert_eq!(frames_in(&bytes), written);
    }

    /// RIFF chunks are word aligned. An odd-length frame that is not padded
    /// shifts everything after it by one byte, and the file stops parsing at
    /// that point rather than failing loudly, so this is the mistake most
    /// likely to produce a file that plays partway and then stops.
    #[test]
    fn an_odd_length_frame_does_not_shift_everything_after_it() {
        let written: Vec<Vec<u8>> = [101, 55, 200].iter().map(|&n| fake_jpeg(n)).collect();
        let (_dir, bytes) = write(&written);
        assert_eq!(frames_in(&bytes), written, "a later frame is misread when padding is wrong");
    }

    /// The size a player reads to know where the file ends.
    #[test]
    fn the_riff_size_matches_the_file() {
        let (_dir, bytes) = write(&[fake_jpeg(60), fake_jpeg(60)]);
        assert_eq!(u32_at(&bytes, 4) as usize, bytes.len() - 8);
    }

    /// Both header copies of the frame count, and the index, have to agree
    /// with how many frames are actually present. A player that trusts the
    /// header will stop early or run off the end.
    #[test]
    fn the_headers_and_index_agree_with_the_frames_present() {
        let written: Vec<Vec<u8>> = (0..7).map(|i| fake_jpeg(40 + i * 3)).collect();
        let (_dir, bytes) = write(&written);

        let avih = 12 + 12 + 8;
        assert_eq!(u32_at(&bytes, avih + 16), 7, "avih dwTotalFrames");
        assert_eq!(u32_at(&bytes, avih + 56 + 8 + 12 + 32), 7, "strh dwLength");

        // The index sits after the movi list and carries 16 bytes per frame.
        let idx = bytes.windows(4).rposition(|w| w == b"idx1").expect("an index");
        assert_eq!(u32_at(&bytes, idx + 4) as usize, 7 * 16, "one index entry per frame");
    }

    /// Index offsets are relative to the `movi` fourcc. Getting this wrong
    /// still plays, because the frames are in order, but breaks seeking,
    /// which is the only thing the index is for.
    #[test]
    fn index_offsets_point_at_the_frames_they_describe() {
        let written: Vec<Vec<u8>> = [77, 140, 63].iter().map(|&n| fake_jpeg(n)).collect();
        let (_dir, bytes) = write(&written);

        let movi = bytes.windows(4).position(|w| w == b"movi").expect("a movi fourcc");
        let idx = bytes.windows(4).rposition(|w| w == b"idx1").expect("an index");

        for (i, frame) in written.iter().enumerate() {
            let entry = idx + 8 + i * 16;
            let offset = u32_at(&bytes, entry + 8) as usize;
            let length = u32_at(&bytes, entry + 12) as usize;
            assert_eq!(length, frame.len(), "frame {i} length");
            assert_eq!(&bytes[movi + offset..movi + offset + 4], b"00dc", "frame {i} offset");
            assert_eq!(&bytes[movi + offset + 8..movi + offset + 8 + length], &frame[..], "frame {i} data");
        }
    }

    /// Some decoders size their input buffer from this and truncate anything
    /// larger, so it has to be the real maximum.
    #[test]
    fn the_suggested_buffer_is_the_largest_frame() {
        let written: Vec<Vec<u8>> = [50, 900, 120].iter().map(|&n| fake_jpeg(n)).collect();
        let (_dir, bytes) = write(&written);
        let avih = 12 + 12 + 8;
        assert_eq!(u32_at(&bytes, avih + 28), 900, "avih dwSuggestedBufferSize");
    }

    /// A file with no frames is degenerate but must not be malformed: the
    /// sizes still have to add up, or it becomes a corrupt file rather than
    /// an empty one.
    #[test]
    fn an_empty_video_is_still_a_valid_file() {
        let (_dir, bytes) = write(&[]);
        assert_eq!(u32_at(&bytes, 4) as usize, bytes.len() - 8);
        assert!(frames_in(&bytes).is_empty());
    }
}
