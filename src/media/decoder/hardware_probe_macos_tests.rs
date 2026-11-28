use std::time::Instant;

use farwing_protocol::session::{Codec, Decoder};

use crate::media::{
    VideoCodec,
    decode_api::{DecoderMode, VideoDecoderConfig},
    decoder::fixtures,
    video_format::{VideoFormatSignature, parse_stream_format},
};
use crate::platform::decoder::{PlatformDecodedFrame, PlatformVideoDecoder};

use super::enumerate_hardware_decoders;

#[test]
#[ignore = "reference capability matrix for Apple M4 / macOS 15.6"]
fn vt_hardware_probe_matrix() {
    use VideoCodec::*;
    let size = (1280, 720);
    let avc = fixtures::sample(H264, 1, 8, size).unwrap();
    let base = parse_stream_format(H264, avc).unwrap();
    // This test must be the first writer of the process-wide bitset. Run the
    // matrix alone or the harness with --test-threads=1; other tests don't probe.
    assert!(!PlatformVideoDecoder::supports_hardware_format(H264, base));

    let start = Instant::now();
    let actual = enumerate_hardware_decoders(|codec, width, height, depth, chroma| {
        let attempt = Instant::now();
        let supported = PlatformVideoDecoder::probe_format(codec, width, height, depth, chroma);
        eprintln!(
            "probe {codec:?} {width}x{height} depth={depth} chroma={chroma}: {supported} ({:.3} ms)",
            attempt.elapsed().as_secs_f64() * 1000.0
        );
        supported
    });
    eprintln!(
        "full enumeration: {:.3} ms",
        start.elapsed().as_secs_f64() * 1000.0
    );
    let expected = [
        (H264, Codec::H264, false, false),
        (H264, Codec::H264, true, false),
        (H265, Codec::Hevc, false, false),
        (H265, Codec::Hevc, false, true),
        (H265, Codec::Hevc, true, false),
        (H265, Codec::Hevc, true, true),
        (Av1, Codec::Av1, false, false),
        (Av1, Codec::Av1, false, true),
    ]
    .map(|(video_codec, codec, chroma444, bit10)| {
        (
            video_codec,
            Decoder {
                codec,
                chroma444,
                bit10,
                width: 3840,
                height: 2160,
                hardware: true,
                av1_visible_size: true,
            },
        )
    });
    assert_eq!(actual, expected);

    // Use a different size from the one advertised: selection matches the
    // format tuple; negotiation, not this predicate, carries the size ceiling.
    for codec in [H264, H265, Av1] {
        for chroma in [1, 3] {
            for depth in [8, 10] {
                let format = VideoFormatSignature {
                    chroma_format_idc: chroma,
                    bit_depth_luma: depth,
                    bit_depth_chroma: depth,
                    ..base
                };
                let supported = !(codec == H264 && depth == 10 || codec == Av1 && chroma == 3);
                assert_eq!(
                    PlatformVideoDecoder::supports_hardware_format(codec, format),
                    supported,
                    "{codec:?} chroma={chroma} depth={depth}"
                );
                assert!(!PlatformVideoDecoder::supports_hardware_format(
                    codec,
                    VideoFormatSignature {
                        bit_depth_chroma: if depth == 8 { 10 } else { 8 },
                        ..format
                    }
                ));
                // Re-run the 4K attempts, including rejected VT sessions and missing fixtures.
                assert_eq!(
                    PlatformVideoDecoder::probe_format(codec, 3840, 2160, depth, chroma),
                    supported,
                    "repeat: {codec:?} chroma={chroma} depth={depth}"
                );
            }
        }
    }

    // An ordinary stream can still open and deliver a frame in this process.
    let mut decoder = PlatformVideoDecoder::open(&VideoDecoderConfig {
        codec: H264,
        width: size.0,
        height: size.1,
        mode: DecoderMode::Hardware,
        gpu_device: None,
        extra_data: Default::default(),
    })
    .unwrap();
    let token = 42;
    decoder
        .push_packet(&fixtures::first_picture(H264, avc), token)
        .unwrap();
    let Some(PlatformDecodedFrame::Gpu(frame)) = decoder.poll_owned_frame().unwrap() else {
        panic!("expected VT hardware output after probing");
    };
    assert_eq!(
        (frame.pts(), frame.width(), frame.height()),
        (token, size.0, size.1)
    );
    assert!(decoder.poll_owned_frame().unwrap().is_none());
    assert!(decoder.poll_dropped_token().is_none());
}
