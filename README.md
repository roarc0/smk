# smk

A pure Rust library for decoding [Smacker Video](http://wiki.multimedia.cx/index.php/Smacker) (.smk) files.

This is a faithful port of [libsmacker](https://libsmacker.sourceforge.net/) 1.2.0. The Rust implementation produces pixel-perfect, bit-identical output compared to the original C library.

## Features

- Open `.smk` files from disk (streaming or fully buffered) or from memory
- Decode video frames (8-bit indexed, all 6 block types including SMKv4 extensions)
- Decode audio tracks (raw PCM and DPCM compression, mono/stereo, 8/16-bit)
- Decode palettes (delta-encoded with skip, copy, and direct-set operations)
- Frame navigation: first, next, seek to keyframe

## Usage

```rust
use smk::{Smk, SmkFrame};

let mut s = Smk::open_file("video.smk", true)?;

let (_, frame_count, usf) = s.info_all();
let (w, h, yscale) = s.info_video();

s.enable_all(0xFF); // enable video + all audio tracks

let mut status = s.first_frame()?;
loop {
    let pixels = s.video_frame();   // &[u8], w*h indexed pixels
    let palette = s.palette();      // &[[u8; 3]; 256], RGB

    // do something with the frame...

    if status == SmkFrame::Done || status == SmkFrame::Last {
        break;
    }
    status = s.next_frame()?;
}
```

## Testing

Run the unit tests (no test files needed):

```
cargo test
```

To test with a real `.smk` file, place it at `testdata/test.smk` and run:

```
cargo test
```

The integration tests will automatically pick it up and verify that all frames decode without errors. If the file is not present, those tests are skipped.

### Extracting frames and audio

An example program extracts BMP frames, WAV audio, and an uncompressed AVI from an `.smk` file:

```
cargo run --example extract
```

This reads `testdata/test.smk` and writes output to `testdata/`. You can also specify paths:

```
cargo run --example extract -- path/to/video.smk output_dir/
```

Output files:
- `frame_NNNN.bmp` - every 10th frame as 8-bit indexed BMP
- `audio_N.wav` - PCM audio for each track
- `output.avi` - uncompressed 24-bit AVI with audio

## License

LGPL-2.1-or-later (same as the original libsmacker).
