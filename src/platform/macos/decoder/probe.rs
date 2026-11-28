//! Real-picture hardware probing and the decode thread's format lookup.

use std::sync::atomic::{
    AtomicU16,
    Ordering::{Acquire, Release},
};

use anyhow::{Context, Result, ensure};
use objc2_core_media::{kCMVideoCodecType_AV1, kCMVideoCodecType_H264, kCMVideoCodecType_HEVC};
use objc2_core_video::CVPixelBufferGetIOSurface;
use objc2_video_toolbox::VTIsHardwareDecodeSupported;

use crate::media::decoder::fixtures;
use crate::platform::surface::sample_pixels;

use super::{
    DecoderMode, GpuFrame, PlatformDecodedFrame, PlatformVideoDecoder, VideoCodec,
    VideoDecoderConfig, VideoFormatSignature, format::PixelFormat, vt::HardwareError,
};

#[derive(Debug, thiserror::Error)]
enum ProbeError {
    #[error("codec has no hardware decoder")]
    NoHardware,
    #[error("no fixture for this format and size")]
    MissingFixture,
    #[error("{operation}: {source}")]
    Decoder {
        operation: &'static str,
        source: HardwareError,
    },
    #[error("first picture produced no GPU frame")]
    MissingGpuFrame,
    #[error("first picture produced more than one frame")]
    ExtraFrame,
    #[error("decoder dropped token {0}")]
    Dropped(i64),
    #[error("{0:#}")]
    Validation(#[from] anyhow::Error),
}

impl ProbeError {
    fn decoder(operation: &'static str, source: impl Into<HardwareError>) -> Self {
        Self::Decoder {
            operation,
            source: source.into(),
        }
    }

    fn level(&self) -> tracing::Level {
        use tracing::Level;
        match self {
            Self::NoHardware | Self::MissingFixture => Level::DEBUG,
            Self::Decoder { source, .. } if source.rejects_format() => Level::DEBUG,
            _ => Level::WARN,
        }
    }
}

#[test]
fn probe_failure_log_levels_distinguish_format_rejection_from_anomalies() {
    use crate::media::decode_api::DecodeError;
    use objc2_video_toolbox::{kVTVideoDecoderBadDataErr, kVTVideoDecoderUnsupportedDataFormatErr};
    use tracing::Level;

    let decoder = |source| ProbeError::Decoder {
        operation: "submit first picture",
        source,
    };
    let status = |operation, status, decode| {
        decoder(HardwareError::VtStatus {
            operation,
            status,
            decode,
        })
    };
    for error in [
        ProbeError::NoHardware,
        ProbeError::MissingFixture,
        decoder(DecodeError::Unsupported.into()),
        // The same VT rejection has different recovery semantics at runtime.
        status(
            "session creation",
            kVTVideoDecoderUnsupportedDataFormatErr,
            DecodeError::HardwareFailure,
        ),
        status(
            "decode",
            kVTVideoDecoderUnsupportedDataFormatErr,
            DecodeError::NeedKeyframe,
        ),
    ] {
        assert_eq!(error.level(), Level::DEBUG, "{error}");
    }
    for error in [
        decoder(DecodeError::HardwareFailure.into()),
        decoder(DecodeError::NeedKeyframe.into()),
        status(
            "decode",
            kVTVideoDecoderBadDataErr,
            DecodeError::NeedKeyframe,
        ),
        decoder(HardwareError::InvalidOutput(
            "GPU frame has no IOSurface".into(),
        )),
        ProbeError::MissingGpuFrame,
        ProbeError::ExtraFrame,
        ProbeError::Dropped(1),
        ProbeError::Validation(anyhow::anyhow!("fixture pattern mismatch")),
    ] {
        assert_eq!(error.level(), Level::WARN, "{error}");
    }
}

// Release publishes successful probe bits; acquire lets the decode thread read
// them without waiting or re-probing. Before sending session.start, negotiation
// joins the probe or takes the already populated inventory under its lock.
// Never clear these bits: a failed retry can leave selection out of sync with
// inventory until a successful probe or process restart. Runtime open/decode
// errors still handle fallback.
static VERIFIED_FORMATS: AtomicU16 = AtomicU16::new(0);

fn format_bit(codec: VideoCodec, chroma: u8, depth: u8) -> Option<u16> {
    // 3 codecs × 4 bits in u16. Check capacity and widen the bitset if more
    // codecs are added beyond the 4 that fit.
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
            .is_some_and(|bit| VERIFIED_FORMATS.load(Acquire) & bit != 0)
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
            VERIFIED_FORMATS.fetch_or(bit, Release);
            true
        }
        Err(error) => {
            if error.level() == tracing::Level::DEBUG {
                tracing::debug!(?codec, width, height, depth, chroma,
                    reason = %error, "VideoToolbox hardware probe failed");
            } else {
                tracing::warn!(?codec, width, height, depth, chroma,
                    reason = %error, "VideoToolbox hardware probe failed");
            }
            false
        }
    }
}

fn verify_fixture(
    codec: VideoCodec,
    (width, height): (u32, u32),
    depth: u8,
    chroma: u8,
) -> Result<u16, ProbeError> {
    let codec_type = match codec {
        VideoCodec::H264 => kCMVideoCodecType_H264,
        VideoCodec::H265 => kCMVideoCodecType_HEVC,
        VideoCodec::Av1 => kCMVideoCodecType_AV1,
    };
    // SAFETY: This query takes only a CoreMedia codec identifier by value.
    if !unsafe { VTIsHardwareDecodeSupported(codec_type) } {
        return Err(ProbeError::NoHardware);
    }
    let sample = fixtures::sample(codec, chroma, depth, (width, height))
        .ok_or(ProbeError::MissingFixture)?;
    let bit = format_bit(codec, chroma, depth).context("unsupported fixture format")?;
    let picture = fixtures::first_picture(codec, sample);
    // The AV1 4:2:0 10-bit fixture alone is full-range. Keep this expectation
    // independent of the decoder's range parser, but use its fourcc mapping.
    let full_range = codec == VideoCodec::Av1 && chroma == 1 && depth == 10;
    let expected = PixelFormat::for_format(chroma, depth, full_range)
        .context("no pixel format for fixture")?;

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
    .map_err(|error| ProbeError::decoder("open hardware decoder", error))?;
    let token = 1;
    decoder
        .push_packet_with_detail(&picture, token)
        .map_err(|error| ProbeError::decoder("submit first picture", error))?;
    if let Some(dropped) = decoder.poll_dropped_token() {
        return Err(ProbeError::Dropped(dropped));
    }
    let Some(PlatformDecodedFrame::Gpu(frame)) = decoder
        .poll_owned_frame()
        .map_err(|error| ProbeError::decoder("poll first picture", error))?
    else {
        return Err(ProbeError::MissingGpuFrame);
    };
    if decoder
        .poll_owned_frame()
        .map_err(|error| ProbeError::decoder("poll remaining output", error))?
        .is_some()
    {
        return Err(ProbeError::ExtraFrame);
    }
    check_frame(&frame, token, (width, height), expected.fourcc)?;
    check_fixture_pattern(&frame, codec, chroma, depth)?;
    Ok(bit)
}

fn check_frame(frame: &GpuFrame, token: i64, size: (u32, u32), fourcc: u32) -> Result<()> {
    ensure!(
        frame.pts() == token,
        "unexpected frame token {}",
        frame.pts()
    );
    ensure!(
        (frame.width(), frame.height()) == size,
        "unexpected frame dimensions {}x{}",
        frame.width(),
        frame.height()
    );
    ensure!(
        CVPixelBufferGetIOSurface(Some(&frame.pixel_buffer)).is_some(),
        "GPU frame has no IOSurface"
    );
    let actual = frame.pixel_format().to_be_bytes();
    let expected = fourcc.to_be_bytes();
    ensure!(
        actual == expected,
        "unexpected pixel format {}, expected {}",
        String::from_utf8_lossy(&actual),
        String::from_utf8_lossy(&expected)
    );
    Ok(())
}

pub(super) fn check_fixture_pattern(
    frame: &GpuFrame,
    codec: VideoCodec,
    chroma: u8,
    depth: u8,
) -> Result<()> {
    let size = (frame.width(), frame.height());
    let points = fixtures::pattern_points(size);
    let pixels = sample_pixels(frame, points)?;
    // Same fixture points, expected YUV values and tolerance as Windows diagnostics.
    for (i, (actual, point)) in pixels.into_iter().zip(points).enumerate() {
        let expected = if codec != VideoCodec::Av1 {
            fixtures::avc_hevc_yuv(i)
        } else if chroma == 3 {
            fixtures::av1_444_yuv(size, point, i)
        } else {
            fixtures::av1_420_yuv(depth, i)
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
