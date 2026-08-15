//! H.264 in MP4, by piping raw frames to FFmpeg when the user happens to have
//! it.
//!
//! The built-in [`crate::viewer::avi`] writer exists so the tool needs nothing
//! installed, and it stays the default for exactly that reason. What it cannot
//! be is small or widely postable: MJPEG stores every frame whole, and a
//! timelapse is mostly the same picture over and over, which is the one thing
//! inter-frame compression is best at. X will not take an AVI at all and
//! Discord will not preview one.
//!
//! So this is strictly additive. Nothing here runs unless FFmpeg is already on
//! the path and the user asked for MP4, and the promise that the tool works
//! with nothing installed is untouched.
//!
//! Frames go in as raw RGB24 over a pipe rather than as JPEG, so the export is
//! encoded once instead of being compressed to JPEG and then recompressed.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};

/// An FFmpeg process being fed raw frames.
pub struct Mp4Writer {
    child: Child,
    stdin: Option<ChildStdin>,
    frames: u32,
    path: PathBuf,
}

impl Mp4Writer {
    pub fn create(path: &Path, width: u32, height: u32, fps: u32) -> std::io::Result<Mp4Writer> {
        let mut command = Command::new("ffmpeg");
        command
            .arg("-hide_banner")
            // Only real errors. FFmpeg's per-frame progress would fight the
            // exporter's own counter for the same line of terminal.
            .args(["-loglevel", "error"])
            .arg("-y")
            // Raw frames arriving on stdin, which is why the size and rate have
            // to be stated: there is no container to read them from.
            .args(["-f", "rawvideo", "-pixel_format", "rgb24"])
            .args(["-video_size", &format!("{width}x{height}")])
            .args(["-framerate", &fps.to_string()])
            .args(["-i", "-"])
            // yuv420p needs even dimensions, and an export size is whatever the
            // user asked for. One pixel of padding is invisible and beats
            // failing after the whole render.
            .args(["-vf", "pad=ceil(iw/2)*2:ceil(ih/2)*2"])
            .args(["-c:v", "libx264", "-preset", "medium", "-crf", "20"])
            // Not the default for H.264, and the difference between a file
            // every player and website accepts and one that many refuse.
            .args(["-pix_fmt", "yuv420p"])
            // Moves the index to the front so the file previews without being
            // downloaded whole, which is the point of making an MP4.
            .args(["-movflags", "+faststart"])
            .arg(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null());

        let mut child = command.spawn().map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!(
                    "could not run ffmpeg to write {}: {e}. Export as AVI instead, which needs nothing installed.",
                    path.display()
                ),
            )
        })?;
        let stdin = child.stdin.take().expect("stdin was piped");
        Ok(Mp4Writer { child, stdin: Some(stdin), frames: 0, path: path.to_path_buf() })
    }

    /// One frame, top-down RGB24, exactly as `to_rgb24` produces it.
    ///
    /// A write failing almost always means FFmpeg has already exited, and its
    /// own message on stderr says why far better than a broken pipe does, so
    /// this reports the frame and lets `finish` collect the real reason.
    pub fn add_frame(&mut self, rgb: &[u8]) -> std::io::Result<()> {
        let stdin = self.stdin.as_mut().expect("add_frame after finish");
        stdin.write_all(rgb).map_err(|e| {
            std::io::Error::new(e.kind(), format!("ffmpeg stopped accepting frames after {} of them: {e}", self.frames))
        })?;
        self.frames += 1;
        Ok(())
    }

    pub fn frames(&self) -> u32 {
        self.frames
    }

    /// Closes the pipe and waits. Dropping stdin is what tells FFmpeg the
    /// stream ended, and without the wait the file is still being written when
    /// the caller goes to measure it.
    pub fn finish(mut self) -> std::io::Result<()> {
        drop(self.stdin.take());
        let status = self.child.wait()?;
        if !status.success() {
            return Err(std::io::Error::other(format!(
                "ffmpeg failed while writing {} (exit {}). The messages above are its own.",
                self.path.display(),
                status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".to_string())
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Skipped where FFmpeg is absent, which is the normal state and the whole
    /// reason this path is optional. CI has no FFmpeg, so this is a local
    /// check rather than a gate.
    fn ffmpeg_or_skip() -> bool {
        if crate::ffmpeg_available() {
            return true;
        }
        eprintln!("skipping: ffmpeg is not on PATH");
        false
    }

    fn frame(width: usize, height: usize, shade: u8) -> Vec<u8> {
        let mut rgb = vec![0u8; width * height * 3];
        for (i, byte) in rgb.iter_mut().enumerate() {
            *byte = if (i / 3) % width < width / 2 { shade } else { 40 };
        }
        rgb
    }

    #[test]
    fn writes_a_playable_file() {
        if !ffmpeg_or_skip() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.mp4");

        let mut writer = Mp4Writer::create(&path, 64, 48, 30).unwrap();
        for i in 0..10 {
            writer.add_frame(&frame(64, 48, 20 * i)).unwrap();
        }
        assert_eq!(writer.frames(), 10);
        writer.finish().unwrap();

        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size > 0, "ffmpeg wrote an empty file");

        // Parsed back rather than trusted: the point of the exercise is a file
        // other software will open.
        let probe = Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "v:0"])
            .args(["-show_entries", "stream=width,height,nb_frames,codec_name"])
            .args(["-of", "csv=p=0"])
            .arg(&path)
            .output();
        if let Ok(out) = probe {
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(text.contains("h264"), "expected H.264, got {text}");
            assert!(text.contains("64,48"), "expected 64x48, got {text}");
        }
    }

    /// An odd size is padded rather than refused, since yuv420p cannot carry
    /// one and the alternative is failing after the whole render.
    #[test]
    fn an_odd_sized_export_still_encodes() {
        if !ffmpeg_or_skip() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("odd.mp4");

        let mut writer = Mp4Writer::create(&path, 65, 47, 30).unwrap();
        for _ in 0..4 {
            writer.add_frame(&frame(65, 47, 200)).unwrap();
        }
        writer.finish().unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() > 0);
    }
}
