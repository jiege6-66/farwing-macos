//! Real-picture hardware probing and the decode thread's format lookup.

use std::sync::atomic::{AtomicU16, Ordering::Relaxed};

use anyhow::{Context, Result, bail, ensure};
use objc2_core_media::{kCMVideoCodecType_AV1, kCMVideoCodecType_H264, kCMVideoCodecType_HEVC};
use objc2_core_video::CVPixelBufferGetIOSurface;
use objc2_video_toolbox::VTIsHardwareDecodeSupported;

use crate::media::decoder::fixtures;
use crate::platform::surface::sample_pixels;

use super::{
    DecoderMode, GpuFrame, PlatformDecodedFrame, PlatformVideoDecoder, VideoCodec,
    VideoDecoderConfig, VideoFormatSignature,
};

// Publish only successes; probe_format always tries the requested size. An empty
// inventory can retry, and format lookup never waits or launches its own probe.
static VERIFIED_FORMATS: AtomicU16 = AtomicU16::new(0);

fn format_bit(codec: VideoCodec, chroma: u8, depth: u8) -> Option<u16> {
    let codec = match codec {
        VideoCodec::H264 => 0,
        VideoCodec::H265 => 1,
        VideoCodec::Av1 => 2,
    };
    let chroma = match chroma {
        1 => 0,
        3 => 1,
        _ => return None,
    };
    let depth = match depth {
        8 => 0,
        10 => 1,
        _ => return None,
    };
    Some(1 << (codec * 4 + chroma * 2 + depth))
}

pub(super) fn supports_hardware_format(codec: VideoCodec, format: VideoFormatSignature) -> bool {
    format.bit_depth_luma == format.bit_depth_chroma
        && format_bit(codec, format.chroma_format_idc, format.bit_depth_luma)
            .is_some_and(|bit| VERIFIED_FORMATS.load(Relaxed) & bit != 0)
}

pub(super) fn probe_format(
    codec: VideoCodec,
    width: u32,
    height: u32,
    depth: u8,
    chroma: u8,
) -> bool {
    match verify_fixture(codec, (width, height), depth, chroma) {
        Ok(bit) => {
            VERIFIED_FORMATS.fetch_or(bit, Relaxed);
            true
        }
        Err(error) => {
            tracing::debug!(?codec, width, height, depth, chroma,
                reason = %format_args!("{error:#}"), "VideoToolbox hardware probe failed");
            false
        }
    }
}

fn verify_fixture(
    codec: VideoCodec,
    (width, height): (u32, u32),
    depth: u8,
    chroma: u8,
) -> Result<u16> {
    let codec_type = match codec {
        VideoCodec::H264 => kCMVideoCodecType_H264,
        VideoCodec::H265 => kCMVideoCodecType_HEVC,
        VideoCodec::Av1 => kCMVideoCodecType_AV1,
    };
    // SAFETY: This query takes only a CoreMedia codec identifier by value.
    ensure!(
        unsafe { VTIsHardwareDecodeSupported(codec_type) },
        "codec has no hardware decoder"
    );
    let sample = fixtures::sample(codec, chroma, depth, (width, height))
        .context("no fixture for this format and size")?;
    let bit = format_bit(codec, chroma, depth).context("unsupported fixture format")?;
    let picture = fixtures::first_picture(codec, sample);
    // Match the fixtures' range too: AV1 4:2:0 10-bit is full-range.
    let expected = match (chroma, depth) {
        (1, 8) => *b"420v",
        (1, 10) if codec == VideoCodec::Av1 => *b"xf20",
        (1, 10) => *b"x420",
        (3, 8) => *b"444v",
        (3, 10) => *b"x444",
        _ => bail!("no pixel format for fixture"),
    };

    // Hardware uses RequireHardwareAcceleratedVideoDecoder, with no fallback.
    // The first AU supplies its own parameter sets/sequence header.
    let mut decoder = PlatformVideoDecoder::open(&VideoDecoderConfig {
        codec,
        width,
        height,
        mode: DecoderMode::Hardware,
        gpu_device: None,
        extra_data: Default::default(),
    })
    .context("open hardware decoder")?;
    let token = 1;
    decoder
        .push_packet(&picture, token)
        .context("submit first picture")?;
    let Some(PlatformDecodedFrame::Gpu(frame)) =
        decoder.poll_owned_frame().context("poll first picture")?
    else {
        bail!("first picture produced no GPU frame");
    };
    ensure!(
        frame.pts() == token,
        "unexpected frame token {}",
        frame.pts()
    );
    ensure!(
        (frame.width(), frame.height()) == (width, height),
        "unexpected frame dimensions {}x{}",
        frame.width(),
        frame.height()
    );
    ensure!(
        CVPixelBufferGetIOSurface(Some(&frame.pixel_buffer)).is_some(),
        "GPU frame has no IOSurface"
    );
    let actual = frame.pixel_format().to_be_bytes();
    ensure!(
        actual == expected,
        "unexpected pixel format {}, expected {}",
        String::from_utf8_lossy(&actual),
        String::from_utf8_lossy(&expected)
    );
    ensure!(
        decoder
            .poll_owned_frame()
            .context("poll remaining output")?
            .is_none()
            && decoder.poll_dropped_token().is_none(),
        "expected exactly one frame and no dropped token"
    );
    check_fixture_pattern(&frame, codec, chroma, depth)?;
    Ok(bit)
}

pub(super) fn check_fixture_pattern(
    frame: &GpuFrame,
    codec: VideoCodec,
    chroma: u8,
    depth: u8,
) -> Result<()> {
    let (w, h) = (frame.width(), frame.height());
    let points = [
        (w / 8, h / 8),
        (w * 7 / 8, h / 8),
        (w / 2, h / 2),
        (w / 8, h * 7 / 8),
        (w * 7 / 8, h * 7 / 8),
    ];
    let pixels = sample_pixels(frame, points)?;
    // Same fixture points, expected YUV values and tolerance as Windows diagnostics.
    for (i, (actual, (x, y))) in pixels.into_iter().zip(points).enumerate() {
        let expected = if codec != VideoCodec::Av1 {
            [if i == 2 { 192 } else { 64 }, 96, 160]
        } else if chroma == 3 {
            [
                if i == 2 { 181 } else { 71 },
                if x < w / 2 { 80 } else { 176 },
                if y < h / 2 { 96 } else { 160 },
            ]
        } else {
            [
                match (depth, i == 2) {
                    (8, false) => 71,
                    (8, true) => 181,
                    (_, false) => 91,
                    (_, true) => 117,
                },
                128,
                128,
            ]
        };
        ensure!(
            actual
                .into_iter()
                .zip(expected)
                .all(|(a, b)| a.abs_diff(b) <= 8),
            "fixture pattern at point {i}: {actual:?} != {expected:?}"
        );
    }
    Ok(())
}
