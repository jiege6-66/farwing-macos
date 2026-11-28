//! Encoded samples for decoder diagnostics and capability probing.

use crate::media::VideoCodec;

pub(crate) fn sample(
    codec: VideoCodec,
    chroma: u8,
    depth: u8,
    size: (u32, u32),
) -> Option<&'static [u8]> {
    macro_rules! sample {
        ($name:literal, $extension:literal) => {
            match size {
                (1280, 720) => Some(
                    include_bytes!(concat!("fixtures/matrix/", $name, "_1280x720", $extension))
                        .as_slice(),
                ),
                (1920, 1080) => Some(
                    include_bytes!(concat!("fixtures/matrix/", $name, "_1920x1080", $extension))
                        .as_slice(),
                ),
                (2560, 1440) => Some(
                    include_bytes!(concat!("fixtures/matrix/", $name, "_2560x1440", $extension))
                        .as_slice(),
                ),
                (3840, 2160) => Some(
                    include_bytes!(concat!("fixtures/matrix/", $name, "_3840x2160", $extension))
                        .as_slice(),
                ),
                _ => None,
            }
        };
    }
    match (codec, chroma, depth) {
        (VideoCodec::Av1, 1, 8) => sample!("av1_8", ".obu"),
        (VideoCodec::Av1, 1, 10) => sample!("av1_10", ".obu"),
        (VideoCodec::Av1, 3, 8) => sample!("av1_444_8", ".obu"),
        (VideoCodec::Av1, 3, 10) => sample!("av1_444_10", ".obu"),

        (VideoCodec::H264, 1, 8) => sample!("h264", ".annexb"),
        (VideoCodec::H264, 3, 8) => sample!("h264_444", ".annexb"),
        (VideoCodec::H265, 1, 8) => sample!("hevc", ".annexb"),
        (VideoCodec::H265, 1, 10) => sample!("main10", ".annexb"),
        (VideoCodec::H265, 3, 8) => sample!("hevc_444", ".annexb"),
        (VideoCodec::H265, 3, 10) => sample!("hevc_444_10", ".annexb"),
        _ => None,
    }
}

/// Probe points in visible coordinates, in diagnostic order (center at index 2).
pub(crate) fn pattern_points((w, h): (u32, u32)) -> [(u32, u32); 5] {
    [
        (w / 8, h / 8),
        (w * 7 / 8, h / 8),
        (w / 2, h / 2),
        (w / 8, h * 7 / 8),
        (w * 7 / 8, h * 7 / 8),
    ]
}

// Expected YUV values in the fixtures' 8-bit-normalized sample domain.
pub(crate) fn avc_hevc_yuv(index: usize) -> [u16; 3] {
    [if index == 2 { 192 } else { 64 }, 96, 160]
}

pub(crate) fn av1_444_yuv((w, h): (u32, u32), (x, y): (u32, u32), index: usize) -> [u16; 3] {
    [
        if index == 2 { 181 } else { 71 },
        if x < w / 2 { 80 } else { 176 },
        if y < h / 2 { 96 } else { 160 },
    ]
}

pub(crate) fn av1_420_yuv(depth: u8, index: usize) -> [u16; 3] {
    let y = match (depth, index == 2) {
        (8, false) => 71,
        (8, true) => 181,
        (_, false) => 91,
        (_, true) => 117,
    };
    [y, 128, 128]
}

/// Take the first picture (including its parameter sets and slices) when an
/// Annex-B fixture has multiple access units. AV1 fixtures are returned as-is.
pub(crate) fn first_picture(codec: VideoCodec, data: &[u8]) -> Vec<u8> {
    if codec == VideoCodec::Av1 {
        return data.to_vec();
    }
    let mut out = Vec::new();
    let mut picture = false;
    for nal in crate::media::video_format::annex_b_units(data) {
        let (vcl, first, aud) = if codec == VideoCodec::H264 {
            let kind = nal[0] & 31;
            (
                matches!(kind, 1 | 5),
                nal.get(1).is_some_and(|v| v & 128 != 0),
                kind == 9,
            )
        } else {
            let kind = (nal[0] >> 1) & 63;
            (
                kind <= 31,
                nal.get(2).is_some_and(|v| v & 128 != 0),
                kind == 35,
            )
        };
        if picture && ((vcl && first) || aud) {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
        picture |= vcl;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::sample;
    use crate::media::{VideoCodec, video_format::parse_stream_format};

    #[test]
    fn matrix_contains_only_the_expected_formats_and_sizes() {
        let mut count = 0;
        for codec in [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1] {
            for chroma in [0, 1, 2, 3, 4] {
                for depth in [8, 9, 10, 12] {
                    for size in [
                        (1280, 720),
                        (1920, 1080),
                        (2560, 1440),
                        (3840, 2160),
                        (1280, 1080),
                    ] {
                        let data = sample(codec, chroma, depth, size);
                        let expected = matches!(chroma, 1 | 3)
                            && (depth == 8 || depth == 10 && codec != VideoCodec::H264)
                            && size != (1280, 1080);
                        assert_eq!(
                            data.is_some(),
                            expected,
                            "{codec:?} {chroma} {depth} {size:?}"
                        );
                        if let Some(data) = data {
                            count += 1;
                            let format = parse_stream_format(codec, data).unwrap();
                            assert_eq!((format.visible_width, format.visible_height), size);
                            assert_eq!(format.chroma_format_idc, chroma);
                            assert_eq!(format.bit_depth_luma, depth);
                            assert_eq!(format.bit_depth_chroma, depth);
                        }
                    }
                }
            }
        }
        assert_eq!(count, 40);
    }
}
