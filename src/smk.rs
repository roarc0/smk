use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use crate::audio::{AudioCompress, AudioInfo, AudioTrack};
use crate::bitstream::BitStream;
use crate::error::{Result, SmkError};
use crate::huff::Huff16;
use crate::video::{Video, YScaleMode};

/// Frame-advance return status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmkFrame {
    Done,
    More,
    Last,
}

// --- Source: file-backed or memory-backed ---

pub(crate) enum Source {
    Memory {
        chunk_data: Vec<Vec<u8>>,
    },
    Disk {
        reader: Option<BufReader<File>>,
        chunk_offset: Vec<u64>,
    },
}

// --- Helper: read little-endian u32 ---

fn read_le_u32(r: &mut dyn SmkRead) -> Result<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

fn read_le_u8(r: &mut dyn SmkRead) -> Result<u8> {
    let mut buf = [0u8; 1];
    r.read_exact(&mut buf)?;
    Ok(buf[0])
}

// --- Reader abstraction ---

pub(crate) trait SmkRead {
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()>;
    /// Current stream position (for disk-mode offset tracking).
    fn stream_position(&mut self) -> Result<u64>;
    /// Seek forward by `n` bytes (for disk-mode frame skipping).
    fn seek_forward(&mut self, n: u64) -> Result<()>;
}

impl SmkRead for BufReader<File> {
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        Read::read_exact(self, buf)?;
        Ok(())
    }
    fn stream_position(&mut self) -> Result<u64> {
        Ok(Seek::stream_position(self)?)
    }
    fn seek_forward(&mut self, n: u64) -> Result<()> {
        Seek::seek(self, SeekFrom::Current(n as i64))?;
        Ok(())
    }
}

/// Wrapper to give a `&[u8]` slice a tracked position for SmkRead.
pub(crate) struct SliceReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> SliceReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        SliceReader { data, pos: 0 }
    }
}

impl SmkRead for SliceReader<'_> {
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        let end = self.pos + buf.len();
        if end > self.data.len() {
            return Err(SmkError::InvalidData("short read"));
        }
        buf.copy_from_slice(&self.data[self.pos..end]);
        self.pos = end;
        Ok(())
    }
    fn stream_position(&mut self) -> Result<u64> {
        Ok(self.pos as u64)
    }
    fn seek_forward(&mut self, n: u64) -> Result<()> {
        let new = self.pos + n as usize;
        if new > self.data.len() {
            return Err(SmkError::InvalidData("seek past end"));
        }
        self.pos = new;
        Ok(())
    }
}

// --- Main Smacker handle ---

pub struct Smk {
    /// Microseconds per frame
    pub(crate) usf: f64,
    /// Total frame count
    pub(crate) frame_count: u32,
    /// Does the file loop?
    pub(crate) ring_frame: bool,
    /// Current frame index
    pub(crate) cur_frame: u32,

    pub(crate) source: Source,
    pub(crate) chunk_size: Vec<u32>,
    pub(crate) keyframe: Vec<bool>,
    pub(crate) frame_type: Vec<u8>,

    pub(crate) video: Video,
    pub(crate) audio: [AudioTrack; 7],
}

impl Smk {
    /// Open an SMK file from disk.
    ///
    /// If `memory_mode` is true, all frame data is loaded into memory and the
    /// file handle is closed. Otherwise the file is kept open for streaming.
    pub fn open_file(path: impl AsRef<Path>, memory_mode: bool) -> Result<Smk> {
        let file = File::open(path.as_ref()).map_err(SmkError::Io)?;
        let mut reader = BufReader::new(file);

        let mut s = Self::open_generic(&mut reader, memory_mode)?;

        if !memory_mode {
            if let Source::Disk {
                reader: ref mut r, ..
            } = s.source
            {
                *r = Some(reader);
            }
        }

        Ok(s)
    }

    /// Open an SMK from an in-memory buffer.
    pub fn open_memory(data: &[u8]) -> Result<Smk> {
        let mut reader = SliceReader::new(data);
        Self::open_generic(&mut reader, true)
    }

    pub fn info_all(&self) -> (u32, u32, f64) {
        (self.cur_frame, self.frame_count, self.usf)
    }

    pub fn info_video(&self) -> (u32, u32, YScaleMode) {
        (self.video.w, self.video.h, self.video.y_scale_mode)
    }

    pub fn info_audio(&self) -> AudioInfo {
        let mut info = AudioInfo {
            track_mask: 0,
            channels: [0; 7],
            bitdepth: [0; 7],
            rate: [0; 7],
        };
        for (i, track) in self.audio.iter().enumerate() {
            if track.exists {
                info.track_mask |= 1 << i;
                info.channels[i] = track.channels;
                info.bitdepth[i] = track.bitdepth;
                info.rate[i] = track.rate;
            }
        }
        info
    }

    pub fn enable_all(&mut self, mask: u8) {
        self.video.enable = mask & 0x80 != 0;
        for i in 0..7 {
            self.audio[i].enable = mask & (1 << i) != 0;
        }
    }

    pub fn enable_video(&mut self, enable: bool) {
        self.video.enable = enable;
    }

    pub fn enable_audio(&mut self, track: u8, enable: bool) {
        if (track as usize) < 7 {
            self.audio[track as usize].enable = enable;
        }
    }

    pub fn palette(&self) -> &[[u8; 3]; 256] {
        &self.video.palette
    }

    pub fn video_frame(&self) -> &[u8] {
        &self.video.frame
    }

    pub fn audio_data(&self, track: u8) -> &[u8] {
        assert!((track as usize) < 7, "audio track index must be 0..6");
        &self.audio[track as usize].buffer
    }

    pub fn audio_size(&self, track: u8) -> u32 {
        assert!((track as usize) < 7, "audio track index must be 0..6");
        self.audio[track as usize].buffer_size
    }

    /// Rewind to the first frame and decode it.
    pub fn first_frame(&mut self) -> Result<SmkFrame> {
        self.cur_frame = 0;
        self.render_frame()?;

        if self.frame_count == 1 {
            Ok(SmkFrame::Last)
        } else {
            Ok(SmkFrame::More)
        }
    }

    /// Advance to the next frame and decode it.
    pub fn next_frame(&mut self) -> Result<SmkFrame> {
        let total = self.frame_count + u32::from(self.ring_frame);

        if self.cur_frame + 1 < total {
            self.cur_frame += 1;
            self.render_frame()?;

            if self.cur_frame + 1 == total {
                Ok(SmkFrame::Last)
            } else {
                Ok(SmkFrame::More)
            }
        } else if self.ring_frame {
            // Loop: jump back to frame 1 (frame 0 is the setup frame).
            self.cur_frame = 1;
            self.render_frame()?;

            if self.cur_frame + 1 == total {
                Ok(SmkFrame::Last)
            } else {
                Ok(SmkFrame::More)
            }
        } else {
            Ok(SmkFrame::Done)
        }
    }

    /// Seek to the keyframe at or before the given frame index, then decode it.
    pub fn seek_keyframe(&mut self, frame: u32) -> Result<()> {
        self.cur_frame = frame;

        // Roll back to the nearest keyframe.
        while self.cur_frame > 0 && !self.keyframe[self.cur_frame as usize] {
            self.cur_frame -= 1;
        }

        self.render_frame()
    }

    /// Core open logic shared by file and memory paths.
    pub(crate) fn open_generic(r: &mut dyn SmkRead, memory_mode: bool) -> Result<Smk> {
        // --- Signature: "SMK" ---
        let mut sig = [0u8; 3];
        r.read_exact(&mut sig)?;
        if &sig != b"SMK" {
            return Err(SmkError::InvalidSignature);
        }

        // --- Version: '2' or '4' ---
        let mut version = read_le_u8(r)?;
        if version != b'2' && version != b'4' {
            eprintln!(
                "smk: warning: invalid SMK version '{}', guessing",
                version as char
            );
            version = if version < b'4' { b'2' } else { b'4' };
        }

        // --- Dimensions ---
        let w = read_le_u32(r)?;
        let h = read_le_u32(r)?;

        // --- Frame count ---
        let frame_count = read_le_u32(r)?;

        // --- Frame rate ---
        let raw_rate = read_le_u32(r)?;
        let temp_l = raw_rate as i32;
        let usf = if temp_l > 0 {
            f64::from(temp_l) * 1000.0
        } else if temp_l < 0 {
            f64::from(temp_l) * -10.0
        } else {
            100_000.0
        };

        // --- Video flags ---
        let flags = read_le_u32(r)?;
        let ring_frame = flags & 0x01 != 0;

        let y_scale_mode = if flags & 0x04 != 0 {
            YScaleMode::Interlace
        } else if flags & 0x02 != 0 {
            YScaleMode::Double
        } else {
            YScaleMode::None
        };

        // --- Audio max buffer sizes (7 tracks) ---
        let mut audio: [AudioTrack; 7] = Default::default();
        for track in &mut audio {
            track.max_buffer = read_le_u32(r)?;
        }

        // --- Huffman tree chunk size ---
        let tree_chunk_size = read_le_u32(r)?;

        // --- Unpacked sizes for 4 huffman trees ---
        let mut tree_size = [0u32; 4];
        for ts in &mut tree_size {
            *ts = read_le_u32(r)?;
        }

        // --- Audio rate data (7 tracks) ---
        for (i, track) in audio.iter_mut().enumerate() {
            let temp_u = read_le_u32(r)?;

            if temp_u & 0x4000_0000 != 0 {
                track.exists = true;
                track.buffer = vec![0u8; track.max_buffer as usize];

                track.compress = if temp_u & 0x0C00_0000 != 0 {
                    eprintln!(
                        "smk: warning: audio track {} uses Bink compression (unsupported)",
                        i
                    );
                    AudioCompress::Bink
                } else if temp_u & 0x8000_0000 != 0 {
                    AudioCompress::Dpcm
                } else {
                    AudioCompress::Raw
                };

                track.bitdepth = if temp_u & 0x2000_0000 != 0 { 16 } else { 8 };
                track.channels = if temp_u & 0x1000_0000 != 0 { 2 } else { 1 };
                track.rate = temp_u & 0x00FF_FFFF;
            }
        }

        // --- Dummy field ---
        let _ = read_le_u32(r)?;

        // --- Frame sizes + keyframes ---
        let total_frames = frame_count + u32::from(ring_frame);
        let mut chunk_size = vec![0u32; total_frames as usize];
        let mut keyframe = vec![false; total_frames as usize];

        for i in 0..total_frames as usize {
            let raw = read_le_u32(r)?;
            keyframe[i] = raw & 0x01 != 0;
            // Bits 0-1 are flags; actual size has those cleared.
            chunk_size[i] = raw & 0xFFFF_FFFC;
        }

        // --- Frame types ---
        let mut frame_type = vec![0u8; total_frames as usize];
        for ft in &mut frame_type {
            *ft = read_le_u8(r)?;
        }

        // --- Huffman trees ---
        let mut hufftree_chunk = vec![0u8; tree_chunk_size as usize];
        r.read_exact(&mut hufftree_chunk)?;

        let mut bs = BitStream::new(&hufftree_chunk);
        let mut trees: [Huff16; 4] = Default::default();
        for (i, tree) in trees.iter_mut().enumerate() {
            *tree = Huff16::build(&mut bs, tree_size[i]).map_err(|e| {
                eprintln!("smk: error building huff16 tree {}: {}", i, e);
                e
            })?;
        }

        // --- Allocate video frame buffer ---
        let frame_buf = vec![0u8; (w as usize) * (h as usize)];

        // --- Read or index frame data ---
        let source = if memory_mode {
            let mut chunk_data = Vec::with_capacity(total_frames as usize);
            for &size in chunk_size.iter().take(total_frames as usize) {
                let mut data = vec![0u8; size as usize];
                r.read_exact(&mut data)?;
                chunk_data.push(data);
            }
            Source::Memory { chunk_data }
        } else {
            let mut chunk_offset = vec![0u64; total_frames as usize];
            for (offset, &size) in chunk_offset
                .iter_mut()
                .zip(chunk_size.iter().take(total_frames as usize))
            {
                *offset = r.stream_position()?;
                r.seek_forward(size as u64)?;
            }
            Source::Disk {
                // Placeholder — caller fills in the real reader after return.
                reader: None,
                chunk_offset,
            }
        };

        let video = Video {
            enable: true,
            w,
            h,
            y_scale_mode,
            version,
            _tree_size: tree_size,
            tree: trees,
            palette: [[0u8; 3]; 256],
            frame: frame_buf,
        };

        Ok(Smk {
            usf,
            frame_count,
            ring_frame,
            cur_frame: 0,
            source,
            chunk_size,
            keyframe,
            frame_type,
            video,
            audio,
        })
    }

    /// Decode the current frame: palette, audio tracks, and video.
    pub(crate) fn render_frame(&mut self) -> Result<()> {
        let idx = self.cur_frame as usize;
        let chunk_sz = self.chunk_size[idx] as usize;

        if chunk_sz == 0 {
            return Ok(());
        }

        // Get the frame data — borrow from memory or read from disk.
        let owned_buf: Vec<u8>;
        let buf: &[u8] = match &mut self.source {
            Source::Memory { chunk_data } => &chunk_data[idx],
            Source::Disk {
                reader,
                chunk_offset,
            } => {
                let r = reader
                    .as_mut()
                    .ok_or(SmkError::InvalidData("no file reader"))?;
                let offset = chunk_offset[idx];
                r.seek(SeekFrom::Start(offset)).map_err(SmkError::Io)?;
                owned_buf = {
                    let mut data = vec![0u8; chunk_sz];
                    Read::read_exact(r, &mut data)?;
                    data
                };
                &owned_buf
            }
        };

        let mut pos = 0;
        let mut remaining = chunk_sz;
        let ftype = self.frame_type[idx];

        // --- Palette record ---
        if ftype & 0x01 != 0 {
            if remaining == 0 {
                return Err(SmkError::InvalidData("no data for palette record"));
            }
            // First byte * 4 = size of palette sub-chunk.
            let pal_size = buf[pos] as usize * 4;
            if pal_size > remaining {
                return Err(SmkError::InvalidData("palette size exceeds chunk"));
            }

            if self.video.enable && pal_size > 1 {
                self.video.render_palette(&buf[pos + 1..pos + pal_size])?;
            }

            pos += pal_size;
            remaining -= pal_size;
        }

        // --- Audio tracks ---
        for track in 0u8..7 {
            if ftype & (0x02 << track) != 0 {
                if remaining < 4 {
                    return Err(SmkError::InvalidData("no data for audio record"));
                }
                // First 4 bytes = sub-chunk size (LE u32).
                let audio_size =
                    u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]])
                        as usize;
                if audio_size > remaining {
                    return Err(SmkError::InvalidData("audio size exceeds chunk"));
                }

                let t = &mut self.audio[track as usize];
                if t.enable && audio_size > 4 {
                    t.render(&buf[pos + 4..pos + audio_size])?;
                }

                pos += audio_size;
                remaining -= audio_size;
            } else {
                self.audio[track as usize].buffer_size = 0;
            }
        }

        // --- Video ---
        if self.video.enable {
            self.video.render_video(&buf[pos..pos + remaining])?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_real_smk_file() {
        let path = std::path::Path::new("testdata/test.smk");
        if !path.exists() {
            eprintln!("testdata/test.smk not found, skipping");
            return;
        }
        let data = std::fs::read(path).unwrap();
        let s = Smk::open_memory(&data).unwrap();

        let (cur, count, usf) = s.info_all();
        eprintln!("frames: {count}, cur: {cur}, usf: {usf}");
        assert!(count > 0);
        assert!(usf > 0.0);

        let (w, h, yscale) = s.info_video();
        eprintln!("video: {w}x{h}, yscale: {yscale:?}");
        assert!(w > 0);
        assert!(h > 0);
        assert_eq!(s.video.frame.len(), (w as usize) * (h as usize));

        let info = s.info_audio();
        eprintln!("audio track_mask: 0x{:02x}", info.track_mask);
        for i in 0..7 {
            if info.track_mask & (1 << i) != 0 {
                eprintln!(
                    "  track {i}: {}ch {}bit {}Hz",
                    info.channels[i], info.bitdepth[i], info.rate[i]
                );
            }
        }
    }

    #[test]
    fn decode_all_frames_real_smk() {
        let path = std::path::Path::new("testdata/test.smk");
        if !path.exists() {
            eprintln!("testdata/test.smk not found, skipping");
            return;
        }
        let data = std::fs::read(path).unwrap();
        let mut s = Smk::open_memory(&data).unwrap();

        let (_, count, _) = s.info_all();
        eprintln!("decoding {} frames...", count);

        let mut status = s.first_frame().unwrap();
        let mut decoded = 1u32;

        // Verify frames decode without error and contain real data.
        let mut total_nonzero = 0usize;

        loop {
            let frame = s.video_frame();
            total_nonzero += frame.iter().filter(|&&b| b != 0).count();

            if status == SmkFrame::Done || status == SmkFrame::Last {
                break;
            }
            status = s.next_frame().unwrap();
            decoded += 1;
        }

        eprintln!("decoded {decoded} frames, total non-zero pixels: {total_nonzero}");
        assert_eq!(decoded, count);
        // The video should have substantial content across all frames.
        assert!(total_nonzero > 0, "all frames were blank");
    }

    /// Build a minimal valid SMK file in memory and open it.
    #[test]
    fn open_memory_minimal() {
        let smk_data = build_minimal_smk(1, 8, 8, false);
        let s = Smk::open_memory(&smk_data).unwrap();
        assert_eq!(s.frame_count, 1);
        assert_eq!(s.video.w, 8);
        assert_eq!(s.video.h, 8);
        assert!(!s.ring_frame);
        assert_eq!(s.video.version, b'4');
        assert_eq!(s.video.frame.len(), 64);
    }

    #[test]
    fn open_memory_ring_frame() {
        let smk_data = build_minimal_smk(3, 16, 16, true);
        let s = Smk::open_memory(&smk_data).unwrap();
        assert_eq!(s.frame_count, 3);
        assert!(s.ring_frame);
        // total_frames = 3 + 1 = 4
        assert_eq!(s.chunk_size.len(), 4);
        assert_eq!(s.keyframe.len(), 4);
        assert_eq!(s.frame_type.len(), 4);
    }

    #[test]
    fn open_memory_bad_signature() {
        let mut data = build_minimal_smk(1, 8, 8, false);
        data[0] = b'X'; // corrupt signature
        assert!(Smk::open_memory(&data).is_err());
    }

    #[test]
    fn info_accessors() {
        let smk_data = build_minimal_smk(5, 320, 200, false);
        let s = Smk::open_memory(&smk_data).unwrap();
        let (cur, count, usf) = s.info_all();
        assert_eq!(cur, 0);
        assert_eq!(count, 5);
        assert!(usf > 0.0);

        let (w, h, yscale) = s.info_video();
        assert_eq!(w, 320);
        assert_eq!(h, 200);
        assert_eq!(yscale, YScaleMode::None);
    }

    #[test]
    fn first_single_frame() {
        let smk_data = build_minimal_smk(1, 8, 8, false);
        let mut s = Smk::open_memory(&smk_data).unwrap();
        assert_eq!(s.first_frame().unwrap(), SmkFrame::Last);
    }

    #[test]
    fn first_next_multi_frame() {
        let smk_data = build_minimal_smk(3, 8, 8, false);
        let mut s = Smk::open_memory(&smk_data).unwrap();
        assert_eq!(s.first_frame().unwrap(), SmkFrame::More);
        assert_eq!(s.cur_frame, 0);
        assert_eq!(s.next_frame().unwrap(), SmkFrame::More);
        assert_eq!(s.cur_frame, 1);
        assert_eq!(s.next_frame().unwrap(), SmkFrame::Last);
        assert_eq!(s.cur_frame, 2);
        assert_eq!(s.next_frame().unwrap(), SmkFrame::Done);
    }

    #[test]
    fn next_loops_with_ring_frame() {
        let smk_data = build_minimal_smk(2, 8, 8, true);
        let mut s = Smk::open_memory(&smk_data).unwrap();
        // total_frames = 3 (2 + ring)
        assert_eq!(s.first_frame().unwrap(), SmkFrame::More);
        assert_eq!(s.next_frame().unwrap(), SmkFrame::More);
        assert_eq!(s.next_frame().unwrap(), SmkFrame::Last);
        // Now next() should loop back to frame 1.
        assert_eq!(s.next_frame().unwrap(), SmkFrame::More);
        assert_eq!(s.cur_frame, 1);
    }

    // -----------------------------------------------------------------------
    // Test helper: builds a minimal valid SMK binary blob
    // -----------------------------------------------------------------------
    fn build_minimal_smk(frames: u32, w: u32, h: u32, ring: bool) -> Vec<u8> {
        let mut out = Vec::new();

        // Signature + version
        out.extend_from_slice(b"SMK4");

        // Width, height, frame count
        out.extend_from_slice(&w.to_le_bytes());
        out.extend_from_slice(&h.to_le_bytes());
        out.extend_from_slice(&frames.to_le_bytes());

        // Frame rate: 33333 usf (~30 fps)
        let rate: i32 = -3333; // *-10 = 33330 usf
        out.extend_from_slice(&(rate as u32).to_le_bytes());

        // Flags: ring_frame bit
        let flags: u32 = if ring { 0x01 } else { 0x00 };
        out.extend_from_slice(&flags.to_le_bytes());

        // Audio max buffer sizes (7 tracks, all 0)
        for _ in 0..7 {
            out.extend_from_slice(&0u32.to_le_bytes());
        }

        // We need to build the hufftree chunk.
        // 4 empty trees: each is bit=0 (no tree) + bit=0 (terminator) = 2 bits.
        // 4 trees = 8 bits = 1 byte.
        let tree_chunk = [0x00u8]; // 8 zero bits = 4 empty trees
        let tree_chunk_size = tree_chunk.len() as u32;

        out.extend_from_slice(&tree_chunk_size.to_le_bytes());

        // Unpacked sizes for 4 trees.
        // Empty tree: alloc_size doesn't matter since bit=0 path doesn't use it.
        // But we still need valid values. The C code does malloc(sizeof(uint))
        // for empty trees and ignores alloc_size. Our Rust code skips validation
        // for the bit=0 path, so any value works.
        for _ in 0..4 {
            out.extend_from_slice(&16u32.to_le_bytes());
        }

        // Audio rate data (7 tracks, all 0 = no track)
        for _ in 0..7 {
            out.extend_from_slice(&0u32.to_le_bytes());
        }

        // Dummy field
        out.extend_from_slice(&0u32.to_le_bytes());

        // Frame sizes + keyframes
        let total_frames = frames + u32::from(ring);
        let chunk_data_size = 0u32; // empty frames
        for _ in 0..total_frames {
            out.extend_from_slice(&chunk_data_size.to_le_bytes());
        }

        // Frame types
        for _ in 0..total_frames {
            out.push(0u8);
        }

        // Huffman tree chunk
        out.extend_from_slice(&tree_chunk);

        // Frame data (all empty, size=0 each — nothing to write)

        out
    }
}
