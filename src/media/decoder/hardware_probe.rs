use crate::media::VideoCodec;
use farwing_protocol::session::Decoder;

/// The sizes a hardware decoder is probed at, largest first.
pub(super) const PROBE_SIZES: [(u32, u32); 3] = [(3840, 2160), (2560, 1440), (1920, 1080)];

fn session_codec(codec: VideoCodec) -> farwing_protocol::session::Codec {
    match codec {
        VideoCodec::H264 => farwing_protocol::session::Codec::H264,
        VideoCodec::H265 => farwing_protocol::session::Codec::Hevc,
        VideoCodec::Av1 => farwing_protocol::session::Codec::Av1,
    }
}

/// Enumerate supported formats in advertising order, keeping the largest size for each.
pub(super) fn enumerate_hardware_decoders(
    mut probe_format: impl FnMut(VideoCodec, u32, u32, u8, u8) -> bool,
) -> Vec<(VideoCodec, Decoder)> {
    let mut capabilities = Vec::new();
    for codec in [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1] {
        for chroma in [1, 3] {
            for depth in [8, 10] {
                for (width, height) in PROBE_SIZES {
                    if probe_format(codec, width, height, depth, chroma) {
                        capabilities.push((
                            codec,
                            Decoder {
                                codec: session_codec(codec),
                                chroma444: chroma == 3,
                                bit10: depth == 10,
                                width,
                                height,
                                hardware: true,
                                av1_visible_size: true,
                            },
                        ));
                        break;
                    }
                }
            }
        }
    }
    capabilities
}

#[cfg(all(test, target_os = "macos"))]
#[path = "hardware_probe_macos_tests.rs"]
mod macos_tests;

#[cfg(test)]
mod tests {
    use super::{Decoder, PROBE_SIZES, VideoCodec, enumerate_hardware_decoders, session_codec};
    use farwing_protocol::session::Codec;

    type Probe = fn(VideoCodec, u32, u32, u8, u8) -> bool;

    fn expected(
        codec: VideoCodec,
        protocol_codec: Codec,
        chroma444: bool,
        bit10: bool,
        (width, height): (u32, u32),
    ) -> (VideoCodec, Decoder) {
        (
            codec,
            Decoder {
                codec: protocol_codec,
                chroma444,
                bit10,
                width,
                height,
                hardware: true,
                av1_visible_size: true,
            },
        )
    }

    fn partial(codec: VideoCodec, width: u32, height: u32, depth: u8, chroma: u8) -> bool {
        matches!(
            (codec, width, height, depth, chroma),
            (VideoCodec::H264, 3840, 2160, 8, 1)
                | (VideoCodec::H265, 2560, 1440, 10, 1)
                | (VideoCodec::H265, 1920, 1080, 10, 1)
                | (VideoCodec::H265, 1920, 1080, 8, 3)
                | (VideoCodec::Av1, 3840, 2160, 10, 3)
        )
    }

    fn smallest_only(_: VideoCodec, width: u32, height: u32, _: u8, _: u8) -> bool {
        (width, height) == (1920, 1080)
    }

    fn all_formats(size: (u32, u32)) -> Vec<(VideoCodec, Decoder)> {
        [
            (VideoCodec::H264, Codec::H264),
            (VideoCodec::H265, Codec::Hevc),
            (VideoCodec::Av1, Codec::Av1),
        ]
        .into_iter()
        .flat_map(|(codec, protocol_codec)| {
            [(false, false), (false, true), (true, false), (true, true)]
                .map(|(chroma444, bit10)| expected(codec, protocol_codec, chroma444, bit10, size))
        })
        .collect()
    }

    #[test]
    fn advertises_all_formats_in_order_when_supported() {
        let mut calls = 0;
        let capabilities = enumerate_hardware_decoders(|_, _, _, _, _| {
            calls += 1;
            true
        });
        assert_eq!(capabilities, all_formats((3840, 2160)));
        assert_eq!(calls, capabilities.len());
    }

    #[test]
    fn advertises_only_supported_formats_at_their_largest_size() {
        assert_eq!(
            enumerate_hardware_decoders(partial),
            [
                expected(VideoCodec::H264, Codec::H264, false, false, (3840, 2160)),
                expected(VideoCodec::H265, Codec::Hevc, false, true, (2560, 1440)),
                expected(VideoCodec::H265, Codec::Hevc, true, false, (1920, 1080)),
                expected(VideoCodec::Av1, Codec::Av1, true, true, (3840, 2160)),
            ]
        );
    }

    #[test]
    fn advertises_nothing_when_no_format_is_supported() {
        assert!(enumerate_hardware_decoders(|_, _, _, _, _| false).is_empty());
    }

    #[test]
    fn falls_back_to_the_smallest_size_for_every_format() {
        assert_eq!(
            enumerate_hardware_decoders(smallest_only),
            all_formats((1920, 1080))
        );
    }

    fn previous_windows_capabilities(probes: &[Probe]) -> Vec<(VideoCodec, Decoder)> {
        // Fake devices provide predicates so the old Windows loop can run as written.
        struct Writer(Probe);
        impl Writer {
            fn device_handle(&self) -> Probe {
                self.0
            }
        }
        struct PlatformVideoDecoder;
        impl PlatformVideoDecoder {
            fn probe_format(
                device: Probe,
                codec: VideoCodec,
                width: u32,
                height: u32,
                depth: u8,
                chroma: u8,
            ) -> bool {
                device(codec, width, height, depth, chroma)
            }
        }
        let writers: Vec<_> = probes.iter().copied().map(Writer).collect();
        let mut capabilities = Vec::new();
        for codec in [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1] {
            for chroma in [1, 3] {
                for depth in [8, 10] {
                    for (width, height) in PROBE_SIZES {
                        if writers.iter().any(|writer| {
                            PlatformVideoDecoder::probe_format(
                                writer.device_handle(),
                                codec,
                                width,
                                height,
                                depth,
                                chroma,
                            )
                        }) {
                            capabilities.push((
                                codec,
                                Decoder {
                                    codec: session_codec(codec),
                                    chroma444: chroma == 3,
                                    bit10: depth == 10,
                                    width,
                                    height,
                                    hardware: true,
                                    av1_visible_size: true,
                                },
                            ));
                            break;
                        }
                    }
                }
            }
        }
        capabilities
    }

    #[test]
    fn matches_previous_windows_capabilities() {
        let cases: &[&[Probe]] = &[
            &[],
            &[|_, _, _, _, _| false],
            &[|_, _, _, _, _| true],
            &[partial],
            &[smallest_only],
            &[smallest_only, partial],
        ];
        for &probes in cases {
            let actual = enumerate_hardware_decoders(|codec, width, height, depth, chroma| {
                probes
                    .iter()
                    .any(|probe| probe(codec, width, height, depth, chroma))
            });
            assert_eq!(actual, previous_windows_capabilities(probes));
        }
    }
}
