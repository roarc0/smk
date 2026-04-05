//! Extract SMK files to BMP frames, WAV audio, and AVI video.
//!
//! Usage: cargo run --example extract [input.smk] [output_dir]
//!
//! Defaults: input = "testdata/test.smk", output_dir = "testdata"

use smk::{FrameStatus, Smk};
use std::fs;
use std::io::{Seek, Write};
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let input = args
        .get(1)
        .map(|s| s.as_str())
        .unwrap_or("testdata/test.smk");
    let outdir = args.get(2).map(|s| s.as_str()).unwrap_or("testdata");

    if !Path::new(input).exists() {
        eprintln!("File not found: {input}");
        std::process::exit(1);
    }

    fs::create_dir_all(outdir).unwrap();

    let data = fs::read(input).unwrap();
    let mut s = Smk::open_memory(&data).unwrap();

    let smk_info = s.info();
    let video = s.info_video();
    let info = s.info_audio();
    let fps = 1_000_000.0 / smk_info.microseconds_per_frame;
    let w = video.width;
    let h = video.height;
    let frame_count = smk_info.frame_count;

    println!("File: {input}");
    println!("Frames: {frame_count}, {fps:.2} fps");
    println!("Video: {w}x{h}, yscale: {:?}", video.y_scale);

    for i in 0..7 {
        if info.track_mask & (1 << i) != 0 {
            println!(
                "Audio track {i}: {}ch {}bit {}Hz",
                info.channels[i], info.bitdepth[i], info.rate[i]
            );
        }
    }

    s.enable_all(0xFF);

    // Collect all frames (as RGB24) and audio.
    let mut frames: Vec<Vec<u8>> = Vec::new();
    let mut palettes: Vec<[[u8; 3]; 256]> = Vec::new();
    let mut audio_bufs: [Vec<u8>; 7] = Default::default();

    let mut status = s.first_frame().unwrap();
    let mut frame_idx = 0u32;

    loop {
        // Store palette and indexed frame for AVI + BMP.
        palettes.push(*s.palette());
        frames.push(s.video_data().to_vec());

        // Save every 10th frame as BMP (+ first and last).
        if frame_idx % 10 == 0 || status == FrameStatus::Last {
            let path = format!("{outdir}/frame_{frame_idx:04}.bmp");
            write_bmp(&path, w, h, s.palette(), s.video_data());
            println!("Wrote {path}");
        }

        // Accumulate audio.
        for track in 0u8..7 {
            if info.track_mask & (1 << track) != 0 {
                if let Some(adata) = s.audio_data(track) {
                    if !adata.is_empty() {
                        audio_bufs[track as usize].extend_from_slice(adata);
                    }
                }
            }
        }

        if status == FrameStatus::Done || status == FrameStatus::Last {
            break;
        }
        status = s.next_frame().unwrap();
        frame_idx += 1;
    }

    // Write WAV files.
    for track in 0u8..7 {
        if info.track_mask & (1 << track) != 0 && !audio_bufs[track as usize].is_empty() {
            let path = format!("{outdir}/audio_{track}.wav");
            write_wav(
                &path,
                info.channels[track as usize],
                info.bitdepth[track as usize],
                info.rate[track as usize],
                &audio_bufs[track as usize],
            );
            println!(
                "Wrote {path} ({} bytes PCM)",
                audio_bufs[track as usize].len()
            );
        }
    }

    // Write AVI.
    let avi_audio = if info.track_mask & 0x01 != 0 {
        Some(AviAudio {
            channels: info.channels[0],
            bitdepth: info.bitdepth[0],
            rate: info.rate[0],
            data: &audio_bufs[0],
        })
    } else {
        None
    };

    let avi_path = format!("{outdir}/output.avi");
    write_avi(&avi_path, w, h, fps, &palettes, &frames, avi_audio.as_ref());
    println!("Wrote {avi_path}");

    println!("Done! Output in {outdir}/");
}

// ---------------------------------------------------------------------------
// BMP writer (8-bit indexed)
// ---------------------------------------------------------------------------

fn write_bmp(path: &str, w: u32, h: u32, palette: &[[u8; 3]; 256], pixels: &[u8]) {
    let row_stride = ((w + 3) & !3) as usize;
    let pixel_data_size = row_stride * h as usize;
    let header_size: u32 = 14 + 40 + 1024;
    let file_size = header_size + pixel_data_size as u32;

    let mut f = fs::File::create(path).unwrap();

    // BITMAPFILEHEADER
    f.write_all(b"BM").unwrap();
    f.write_all(&file_size.to_le_bytes()).unwrap();
    f.write_all(&0u32.to_le_bytes()).unwrap(); // reserved
    f.write_all(&header_size.to_le_bytes()).unwrap();

    // BITMAPINFOHEADER
    f.write_all(&40u32.to_le_bytes()).unwrap();
    f.write_all(&w.to_le_bytes()).unwrap();
    f.write_all(&h.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap(); // planes
    f.write_all(&8u16.to_le_bytes()).unwrap(); // bpp
    f.write_all(&0u32.to_le_bytes()).unwrap(); // compression
    f.write_all(&(pixel_data_size as u32).to_le_bytes())
        .unwrap();
    f.write_all(&2835u32.to_le_bytes()).unwrap(); // xppm
    f.write_all(&2835u32.to_le_bytes()).unwrap(); // yppm
    f.write_all(&256u32.to_le_bytes()).unwrap();
    f.write_all(&256u32.to_le_bytes()).unwrap();

    // Palette (BGRA)
    for c in palette {
        f.write_all(&[c[2], c[1], c[0], 0]).unwrap();
    }

    // Pixel rows bottom-to-top
    let wu = w as usize;
    let mut row_buf = vec![0u8; row_stride];
    for y in (0..h as usize).rev() {
        row_buf[..wu].copy_from_slice(&pixels[y * wu..(y + 1) * wu]);
        f.write_all(&row_buf).unwrap();
    }
}

// ---------------------------------------------------------------------------
// WAV writer
// ---------------------------------------------------------------------------

fn write_wav(path: &str, channels: u8, bitdepth: u8, rate: u32, data: &[u8]) {
    let bps = (bitdepth / 8) as u16;
    let block_align = channels as u16 * bps;
    let byte_rate = rate * block_align as u32;
    let data_size = data.len() as u32;

    let mut f = fs::File::create(path).unwrap();
    f.write_all(b"RIFF").unwrap();
    f.write_all(&(36 + data_size).to_le_bytes()).unwrap();
    f.write_all(b"WAVE").unwrap();
    f.write_all(b"fmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
    f.write_all(&(channels as u16).to_le_bytes()).unwrap();
    f.write_all(&rate.to_le_bytes()).unwrap();
    f.write_all(&byte_rate.to_le_bytes()).unwrap();
    f.write_all(&block_align.to_le_bytes()).unwrap();
    f.write_all(&(bitdepth as u16).to_le_bytes()).unwrap();
    f.write_all(b"data").unwrap();
    f.write_all(&data_size.to_le_bytes()).unwrap();
    f.write_all(data).unwrap();
}

// ---------------------------------------------------------------------------
// AVI writer (uncompressed 8-bit paletted video + optional PCM audio)
// ---------------------------------------------------------------------------
//
// AVI format (RIFF):
//   RIFF 'AVI '
//     LIST 'hdrl'
//       avih (main header)
//       LIST 'strl' (video stream)
//         strh (stream header)
//         strf (stream format = BITMAPINFOHEADER + palette)
//       LIST 'strl' (audio stream, optional)
//         strh
//         strf (WAVEFORMATEX)
//     LIST 'movi'
//       00dc (video chunks)  or  01wb (audio chunks)
//     idx1 (index)

struct AviAudio<'a> {
    channels: u8,
    bitdepth: u8,
    rate: u32,
    data: &'a [u8],
}

fn write_avi(
    path: &str,
    w: u32,
    h: u32,
    fps: f64,
    palettes: &[[[u8; 3]; 256]],
    frames: &[Vec<u8>],
    audio: Option<&AviAudio>,
) {
    let nframes = frames.len() as u32;
    let usf = (1_000_000.0 / fps) as u32;
    let has_audio = audio.is_some();
    let nstreams: u32 = if has_audio { 2 } else { 1 };

    // 24-bit BGR: each row padded to 4 bytes.
    let rgb_row_bytes = w * 3;
    let rgb_row_stride = (rgb_row_bytes + 3) & !3;
    let frame_size = rgb_row_stride * h;

    // Pre-compute audio chunk sizes: split total audio evenly across frames.
    let (audio_block_align, audio_rate, audio_chunks) = if let Some(a) = audio {
        let ba = a.channels as u32 * (a.bitdepth as u32 / 8);
        let total = a.data.len() as u32;
        let per_frame = (total / nframes / ba) * ba;
        let mut chunks = Vec::new();
        let mut off = 0u32;
        for i in 0..nframes {
            let sz = if i == nframes - 1 {
                total - off
            } else {
                per_frame
            };
            chunks.push((off, sz));
            off += sz;
        }
        (ba as u16, a.rate, chunks)
    } else {
        (0, 0, Vec::new())
    };

    // Compute movi sizes and index entries.
    let vchunk_data_size = frame_size;
    let vchunk_size = 8 + vchunk_data_size;

    let mut movi_content_size: u32 = 0;
    let mut idx_entries: Vec<(u32, u32)> = Vec::new();

    for i in 0..nframes as usize {
        idx_entries.push((movi_content_size, vchunk_data_size));
        movi_content_size += vchunk_size;

        if has_audio {
            let (_, asz) = audio_chunks[i];
            idx_entries.push((movi_content_size, asz));
            movi_content_size += 8 + asz + (asz & 1);
        }
    }

    let movi_list_size = 4 + movi_content_size;
    let idx1_size = (idx_entries.len() * 16) as u32;

    // Header sizes.
    let avih_size = 56u32;
    let video_strh_size = 56u32;
    let video_strf_size = 40u32; // 24-bit: no palette in header
    let video_strl_size = 4 + (8 + video_strh_size) + (8 + video_strf_size);

    let mut hdrl_size = 4 + (8 + avih_size) + (8 + video_strl_size);

    let (audio_strh_size, audio_strf_size, audio_strl_size);
    if has_audio {
        audio_strh_size = 56u32;
        audio_strf_size = 18u32;
        audio_strl_size = 4 + (8 + audio_strh_size) + (8 + audio_strf_size);
        hdrl_size += 8 + audio_strl_size;
    } else {
        audio_strh_size = 0;
        audio_strf_size = 0;
        audio_strl_size = 0;
    }

    let riff_size = 4 + (8 + hdrl_size) + (8 + movi_list_size) + (8 + idx1_size);

    let mut f = fs::File::create(path).unwrap();

    // RIFF AVI
    f.write_all(b"RIFF").unwrap();
    f.write_all(&riff_size.to_le_bytes()).unwrap();
    f.write_all(b"AVI ").unwrap();

    // LIST hdrl
    f.write_all(b"LIST").unwrap();
    f.write_all(&hdrl_size.to_le_bytes()).unwrap();
    f.write_all(b"hdrl").unwrap();

    // avih
    f.write_all(b"avih").unwrap();
    f.write_all(&avih_size.to_le_bytes()).unwrap();
    f.write_all(&usf.to_le_bytes()).unwrap();
    f.write_all(&0u32.to_le_bytes()).unwrap(); // dwMaxBytesPerSec
    f.write_all(&0u32.to_le_bytes()).unwrap(); // dwPaddingGranularity
    f.write_all(&0x30u32.to_le_bytes()).unwrap(); // flags: HASINDEX | MUSTUSEINDEX
    f.write_all(&nframes.to_le_bytes()).unwrap();
    f.write_all(&0u32.to_le_bytes()).unwrap(); // dwInitialFrames
    f.write_all(&nstreams.to_le_bytes()).unwrap();
    f.write_all(&frame_size.to_le_bytes()).unwrap(); // dwSuggestedBufferSize
    f.write_all(&w.to_le_bytes()).unwrap();
    f.write_all(&h.to_le_bytes()).unwrap();
    f.write_all(&[0u8; 16]).unwrap(); // reserved

    // LIST strl (video)
    f.write_all(b"LIST").unwrap();
    f.write_all(&video_strl_size.to_le_bytes()).unwrap();
    f.write_all(b"strl").unwrap();

    // strh (video)
    f.write_all(b"strh").unwrap();
    f.write_all(&video_strh_size.to_le_bytes()).unwrap();
    f.write_all(b"vids").unwrap();
    f.write_all(&0u32.to_le_bytes()).unwrap(); // fccHandler (uncompressed)
    f.write_all(&0u32.to_le_bytes()).unwrap(); // dwFlags
    f.write_all(&0u16.to_le_bytes()).unwrap(); // wPriority
    f.write_all(&0u16.to_le_bytes()).unwrap(); // wLanguage
    f.write_all(&0u32.to_le_bytes()).unwrap(); // dwInitialFrames
    f.write_all(&usf.to_le_bytes()).unwrap(); // dwScale
    f.write_all(&1_000_000u32.to_le_bytes()).unwrap(); // dwRate
    f.write_all(&0u32.to_le_bytes()).unwrap(); // dwStart
    f.write_all(&nframes.to_le_bytes()).unwrap(); // dwLength
    f.write_all(&frame_size.to_le_bytes()).unwrap(); // dwSuggestedBufferSize
    f.write_all(&0xFFFFFFFFu32.to_le_bytes()).unwrap(); // dwQuality
    f.write_all(&0u32.to_le_bytes()).unwrap(); // dwSampleSize
    f.write_all(&0u16.to_le_bytes()).unwrap(); // rcFrame
    f.write_all(&0u16.to_le_bytes()).unwrap();
    f.write_all(&(w as u16).to_le_bytes()).unwrap();
    f.write_all(&(h as u16).to_le_bytes()).unwrap();

    // strf (video) — BITMAPINFOHEADER, 24-bit, no palette
    f.write_all(b"strf").unwrap();
    f.write_all(&video_strf_size.to_le_bytes()).unwrap();
    f.write_all(&40u32.to_le_bytes()).unwrap(); // biSize
    f.write_all(&w.to_le_bytes()).unwrap();
    f.write_all(&h.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap(); // biPlanes
    f.write_all(&24u16.to_le_bytes()).unwrap(); // biBitCount
    f.write_all(&0u32.to_le_bytes()).unwrap(); // biCompression
    f.write_all(&frame_size.to_le_bytes()).unwrap(); // biSizeImage
    f.write_all(&0u32.to_le_bytes()).unwrap(); // biXPelsPerMeter
    f.write_all(&0u32.to_le_bytes()).unwrap(); // biYPelsPerMeter
    f.write_all(&0u32.to_le_bytes()).unwrap(); // biClrUsed
    f.write_all(&0u32.to_le_bytes()).unwrap(); // biClrImportant

    // LIST strl (audio), optional
    if let Some(a) = audio {
        let ba = audio_block_align;
        let byte_rate = audio_rate * ba as u32;
        let total_samples = a.data.len() as u32 / ba as u32;

        f.write_all(b"LIST").unwrap();
        f.write_all(&audio_strl_size.to_le_bytes()).unwrap();
        f.write_all(b"strl").unwrap();

        f.write_all(b"strh").unwrap();
        f.write_all(&audio_strh_size.to_le_bytes()).unwrap();
        f.write_all(b"auds").unwrap();
        f.write_all(&1u32.to_le_bytes()).unwrap(); // PCM
        f.write_all(&0u32.to_le_bytes()).unwrap();
        f.write_all(&0u16.to_le_bytes()).unwrap();
        f.write_all(&0u16.to_le_bytes()).unwrap();
        f.write_all(&0u32.to_le_bytes()).unwrap();
        f.write_all(&1u32.to_le_bytes()).unwrap(); // dwScale
        f.write_all(&audio_rate.to_le_bytes()).unwrap();
        f.write_all(&0u32.to_le_bytes()).unwrap();
        f.write_all(&total_samples.to_le_bytes()).unwrap();
        f.write_all(&byte_rate.to_le_bytes()).unwrap();
        f.write_all(&0xFFFFFFFFu32.to_le_bytes()).unwrap();
        f.write_all(&(ba as u32).to_le_bytes()).unwrap();
        f.write_all(&0u32.to_le_bytes()).unwrap();
        f.write_all(&0u32.to_le_bytes()).unwrap();

        f.write_all(b"strf").unwrap();
        f.write_all(&audio_strf_size.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
        f.write_all(&(a.channels as u16).to_le_bytes()).unwrap();
        f.write_all(&audio_rate.to_le_bytes()).unwrap();
        f.write_all(&byte_rate.to_le_bytes()).unwrap();
        f.write_all(&ba.to_le_bytes()).unwrap();
        f.write_all(&(a.bitdepth as u16).to_le_bytes()).unwrap();
        f.write_all(&0u16.to_le_bytes()).unwrap(); // cbSize
    }

    // LIST movi
    f.write_all(b"LIST").unwrap();
    f.write_all(&movi_list_size.to_le_bytes()).unwrap();
    f.write_all(b"movi").unwrap();

    let wu = w as usize;
    let stride = rgb_row_stride as usize;
    let mut row_buf = vec![0u8; stride];

    for i in 0..nframes as usize {
        // Video chunk: 00dc — 24-bit BGR pixels, bottom-to-top
        f.write_all(b"00dc").unwrap();
        f.write_all(&vchunk_data_size.to_le_bytes()).unwrap();

        let pal = &palettes[i];
        let pixels = &frames[i];

        for y in (0..h as usize).rev() {
            let src_row = &pixels[y * wu..(y + 1) * wu];
            for (x, &idx) in src_row.iter().enumerate() {
                let c = &pal[idx as usize];
                let off = x * 3;
                row_buf[off] = c[2]; // B
                row_buf[off + 1] = c[1]; // G
                row_buf[off + 2] = c[0]; // R
            }
            // Padding bytes stay zero.
            f.write_all(&row_buf).unwrap();
        }

        // Audio chunk
        if has_audio {
            let (aoff, asz) = audio_chunks[i];
            f.write_all(b"01wb").unwrap();
            f.write_all(&asz.to_le_bytes()).unwrap();
            f.write_all(&audio.unwrap().data[aoff as usize..(aoff + asz) as usize])
                .unwrap();
            if asz & 1 != 0 {
                f.write_all(&[0u8]).unwrap();
            }
        }
    }

    // idx1
    f.write_all(b"idx1").unwrap();
    f.write_all(&idx1_size.to_le_bytes()).unwrap();

    let mut entry_idx = 0;
    for _ in 0..nframes as usize {
        let (off, sz) = idx_entries[entry_idx];
        f.write_all(b"00dc").unwrap();
        f.write_all(&0x10u32.to_le_bytes()).unwrap(); // AVIIF_KEYFRAME
        f.write_all(&(off + 4).to_le_bytes()).unwrap();
        f.write_all(&sz.to_le_bytes()).unwrap();
        entry_idx += 1;

        if has_audio {
            let (off, sz) = idx_entries[entry_idx];
            f.write_all(b"01wb").unwrap();
            f.write_all(&0x10u32.to_le_bytes()).unwrap();
            f.write_all(&(off + 4).to_le_bytes()).unwrap();
            f.write_all(&sz.to_le_bytes()).unwrap();
            entry_idx += 1;
        }
    }

    let actual = f.stream_position().unwrap();
    let expected = 8 + riff_size as u64;
    assert_eq!(
        actual, expected,
        "AVI file size mismatch: {actual} != {expected}"
    );
}
