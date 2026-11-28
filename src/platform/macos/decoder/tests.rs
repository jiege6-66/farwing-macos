use super::*;
use crate::media::{decoder::fixtures, video_format::annex_b_units};
use crate::platform::surface::SurfaceWriter;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(super) fn config(
    codec: VideoCodec,
    (width, height): (u32, u32),
    mode: DecoderMode,
) -> VideoDecoderConfig {
    VideoDecoderConfig {
        codec,
        width,
        height,
        mode,
        gpu_device: None,
        extra_data: Bytes::new(),
    }
}

// Check immediately, not with a poll/retry loop: the viewer parks when idle.
pub(super) fn terminal(
    decoder: &mut PlatformVideoDecoder,
    token: i64,
) -> Option<PlatformDecodedFrame> {
    let frame = decoder.poll_owned_frame().unwrap();
    let dropped = decoder.poll_dropped_token();
    match &frame {
        Some(PlatformDecodedFrame::Gpu(frame)) => assert_eq!((frame.pts(), dropped), (token, None)),
        Some(PlatformDecodedFrame::Cpu(frame)) => assert_eq!((frame.pts, dropped), (token, None)),
        None => assert_eq!(dropped, Some(token)),
    }
    assert!(decoder.poll_owned_frame().unwrap().is_none());
    assert_eq!(decoder.poll_dropped_token(), None);
    frame
}

fn fixture(codec: VideoCodec, chroma: u8, depth: u8, size: (u32, u32)) -> Vec<u8> {
    fixtures::first_picture(codec, fixtures::sample(codec, chroma, depth, size).unwrap())
}

fn extra(codec: VideoCodec, au: &[u8]) -> Bytes {
    if codec == VideoCodec::Av1 {
        use farwing_codec::syntax::av1::obu::Units;
        return Bytes::copy_from_slice(
            Units::with_unsized_last(au)
                .unwrap()
                .map(Result::unwrap)
                .find(|obu| obu.kind == 1)
                .unwrap()
                .bytes,
        );
    }
    annex_b_units(au)
        .into_iter()
        .filter(|nal| match codec {
            VideoCodec::H264 => matches!(nal[0] & 31, 7 | 8),
            _ => matches!((nal[0] >> 1) & 63, 32..=34),
        })
        .flat_map(|nal| [0, 0, 0, 1].into_iter().chain(nal.iter().copied()))
        .collect::<Vec<_>>()
        .into()
}

#[test]
#[ignore = "requires VideoToolbox hardware; run on an Apple Silicon Mac with AV1 support"]
fn vt_fixture_matrix() {
    use VideoCodec::*;
    let mut failures = Vec::new();
    for (codec, chroma, depth, expected_fourcc) in [
        (H264, 1, 8, *b"420v"),
        (H264, 3, 8, *b"444v"),
        (H265, 1, 8, *b"420v"),
        (H265, 1, 10, *b"x420"),
        (H265, 3, 8, *b"444v"),
        (H265, 3, 10, *b"x444"),
        (Av1, 1, 8, *b"420v"),
        (Av1, 1, 10, *b"xf20"),
        (Av1, 3, 8, *b"444v"),
        (Av1, 3, 10, *b"x444"),
    ] {
        for size in [(1280, 720), (1920, 1080), (2560, 1440), (3840, 2160)] {
            let label = format!("{codec:?} chroma={chroma} depth={depth} {size:?}");
            let au = fixture(codec, chroma, depth, size);
            let mut cfg = config(codec, size, DecoderMode::Hardware);
            cfg.extra_data = extra(codec, &au);
            let mut decoder = match PlatformVideoDecoder::open(&cfg) {
                Ok(decoder) => decoder,
                Err(DecodeError::Unsupported) if codec == Av1 && chroma == 3 => {
                    // M4/macOS 15.6 rejects the AV1 High profile. If a newer VT
                    // accepts it, require the same frame validation as the other cases.
                    eprintln!("{label}: open rejected: Unsupported");
                    let mut lazy =
                        PlatformVideoDecoder::open(&config(codec, size, DecoderMode::Hardware))
                            .unwrap();
                    let result = lazy.push_packet(&au, 0);
                    assert!(terminal(&mut lazy, 0).is_none());
                    assert_eq!(result, Err(DecodeError::Unsupported));
                    continue;
                }
                Err(error) => {
                    eprintln!("{label}: open rejected: {error:?}");
                    failures.push(label);
                    continue;
                }
            };
            let result = decoder.push_packet(&au, 0);
            let frame = terminal(&mut decoder, 0);
            let Some(PlatformDecodedFrame::Gpu(frame)) = frame else {
                eprintln!("{label}: no GPU frame, push={result:?}");
                failures.push(label);
                continue;
            };
            result.unwrap();
            let fourcc = frame.pixel_format().to_be_bytes();
            eprintln!(
                "{label}: {} pts={} pattern",
                String::from_utf8_lossy(&fourcc),
                frame.pts()
            );
            assert_eq!(fourcc, expected_fourcc, "{label}");
            assert_eq!((frame.width(), frame.height()), size);
            let (w, h) = size;
            probe::check_fixture_pattern(&frame, codec, chroma, depth)
                .unwrap_or_else(|error| panic!("{label}: {error:#}"));
            let surface = SurfaceWriter::new()
                .unwrap()
                .wrap_decoded_surface(frame)
                .unwrap();
            assert!(surface.luma_hash(w, h).is_ok());
        }
    }
    assert!(failures.is_empty(), "rejected: {failures:?}");
}

#[test]
#[ignore = "requires VideoToolbox hardware"]
fn h264_hardware_software_luma_match() {
    check_h264_luma_match(1, *b"420v", CpuFormat::Nv12);
}

#[test]
#[ignore = "requires VideoToolbox hardware with H.264 4:4:4 support"]
fn h264_444_hardware_software_luma_match() {
    check_h264_luma_match(3, *b"444v", CpuFormat::Ayuv);
}

fn check_h264_luma_match(chroma: u8, fourcc: [u8; 4], cpu_format: CpuFormat) {
    let size = (1920, 1080);
    let au = fixture(VideoCodec::H264, chroma, 8, size);
    let writer = SurfaceWriter::new().unwrap();
    let mut hashes = Vec::new();
    for mode in [DecoderMode::Hardware, DecoderMode::Software] {
        let mut decoder =
            PlatformVideoDecoder::open(&config(VideoCodec::H264, size, mode)).unwrap();
        decoder.push_packet(&au, i64::MIN).unwrap();
        let surface = match terminal(&mut decoder, i64::MIN).unwrap() {
            PlatformDecodedFrame::Gpu(frame) if mode == DecoderMode::Hardware => {
                assert_eq!((frame.width(), frame.height()), size);
                assert_eq!(frame.pixel_format().to_be_bytes(), fourcc);
                writer.wrap_decoded_surface(frame)
            }
            PlatformDecodedFrame::Cpu(frame) if mode == DecoderMode::Software => {
                assert_eq!(
                    (frame.width, frame.height, frame.format),
                    (size.0, size.1, cpu_format)
                );
                match cpu_format {
                    CpuFormat::Nv12 => assert_eq!(&frame.data[..8], &[64; 8]),
                    CpuFormat::Ayuv => assert_eq!(&frame.data[..4], &[160, 96, 64, 255]),
                    _ => unreachable!(),
                }
                writer.upload_cpu(&frame)
            }
            _ => panic!("unexpected backend for {mode:?}"),
        }
        .unwrap();
        drop(decoder); // The surface owns its decoded memory.
        let hash = std::thread::spawn(move || surface.luma_hash(size.0, size.1).unwrap())
            .join()
            .unwrap();
        eprintln!("H.264 chroma={chroma} {mode:?} luma_hash={hash:016x}");
        hashes.push(hash);
    }
    assert_eq!(hashes[0], hashes[1]);
}

#[test]
#[ignore = "requires VideoToolbox hardware"]
fn vt_10_bit_luma_is_msb_aligned() {
    let size = (1280, 720);
    // FFmpeg software decode of both fixtures gives 255, 769 here; the
    // low two bits are nonzero, so this also distinguishes 8-bit expansion.
    let points = [(318, 179), (331, 180)];
    for (chroma, fourcc) in [(1, *b"x420"), (3, *b"x444")] {
        let mut decoder =
            PlatformVideoDecoder::open(&config(VideoCodec::H265, size, DecoderMode::Hardware))
                .unwrap();
        decoder
            .push_packet(&fixture(VideoCodec::H265, chroma, 10, size), 0)
            .unwrap();
        let Some(PlatformDecodedFrame::Gpu(frame)) = terminal(&mut decoder, 0) else {
            panic!("no 10-bit GPU frame");
        };
        assert_eq!(frame.pixel_format().to_be_bytes(), fourcc);
        let words = crate::platform::surface::test_luma_words(&frame, points).unwrap();
        eprintln!(
            "{} raw little-endian Y words: {words:04x?}",
            std::str::from_utf8(&fourcc).unwrap()
        );
        assert_eq!(words, [255 << 6, 769 << 6]);
    }
}

#[test]
#[ignore = "requires VideoToolbox hardware with AV1 support"]
fn av1_waits_for_a_shown_keyframe_with_in_band_sequence() {
    use crate::media::av1::{assemble, payloads, units};
    let codec = VideoCodec::Av1;
    let size = (1280, 720);
    let source = fixture(codec, 1, 8, size);
    let mut cfg = config(codec, size, DecoderMode::Hardware);
    cfg.extra_data = extra(codec, &source);
    // Exercise the host packetizer/receiver contract as well as VT.
    let mut records = Vec::new();
    for packet in payloads(1200, &source, true).unwrap() {
        records.extend_from_slice(&(packet.len() as u32).to_le_bytes());
        records.extend_from_slice(&packet);
    }
    let in_band = assemble(&records).unwrap();
    let mut headerless = Vec::new();
    for unit in units(&in_band).unwrap().map(Result::unwrap) {
        if unit.kind != 1 {
            headerless.extend_from_slice(unit.bytes);
        }
    }
    let mut decoder = PlatformVideoDecoder::open(&cfg).unwrap();
    for token in [0, 2] {
        assert_eq!(
            decoder.push_packet(&headerless, token),
            Err(DecodeError::NeedKeyframe)
        );
        assert!(terminal(&mut decoder, token).is_none());
        decoder.push_packet(&in_band, token + 1).unwrap();
        assert!(matches!(
            terminal(&mut decoder, token + 1),
            Some(PlatformDecodedFrame::Gpu(_))
        ));
        decoder.reset_for_keyframe().unwrap();
    }
}

#[test]
fn software_decoder_rejects_non_h264() {
    for codec in [VideoCodec::H265, VideoCodec::Av1] {
        assert!(matches!(
            PlatformVideoDecoder::open(&config(codec, (1280, 720), DecoderMode::Software)),
            Err(DecodeError::Unsupported)
        ));
    }
}

fn encode_h264(size: (u32, u32)) -> Vec<Bytes> {
    use farwing_codec::{
        Format,
        encoder::{Config, Encoder},
    };
    let (width, height) = size;
    let mut encoder = Encoder::new(Config {
        width,
        height,
        format: Format {
            codec: VideoCodec::H264,
            chroma: 1,
            depth: 8,
        },
        fps: 30,
        bitrate: 500_000,
    })
    .unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    (0..4)
        .map(|i| {
            // Make a small local change, not a scene cut that triggers another IDR.
            let mut pixels = vec![64; (width * height * 4) as usize];
            pixels[i * 64..(i + 1) * 64].fill(128);
            encoder
                .prepare(&pixels, (width * 4) as usize, &cancel)
                .unwrap();
            let packet = encoder
                .encode(i as i64 * 333_333, i == 0, &cancel)
                .unwrap()
                .unwrap();
            assert_eq!(packet.keyframe, i == 0);
            packet.data.into()
        })
        .collect()
}

#[test]
#[ignore = "requires VideoToolbox hardware"]
fn h264_stream_switch_reset_errors_and_cancellation() {
    let codec = VideoCodec::H264;
    let (small, large) = ((160, 128), (192, 144));
    let first = encode_h264(small);
    let second = encode_h264(large);
    for mode in [DecoderMode::Hardware, DecoderMode::Software] {
        let mut decoder = PlatformVideoDecoder::open(&config(codec, small, mode)).unwrap();
        assert_eq!(
            decoder.push_packet(&first[1], 0),
            Err(DecodeError::NeedKeyframe)
        );
        assert!(terminal(&mut decoder, 0).is_none());
        for (i, au) in first.iter().chain(&second).enumerate() {
            if mode == DecoderMode::Hardware && i == first.len() {
                // The session has not yet adopted this parameter-only update.
                decoder.push_packet(&extra(codec, au), -1).unwrap();
                assert!(terminal(&mut decoder, -1).is_none());
            }
            let token = i as i64 + 1;
            decoder.push_packet(au, token).unwrap();
            let size = if i < first.len() { small } else { large };
            match terminal(&mut decoder, token).unwrap() {
                PlatformDecodedFrame::Gpu(frame) => {
                    assert_eq!((frame.width(), frame.height()), size)
                }
                PlatformDecodedFrame::Cpu(frame) => assert_eq!((frame.width, frame.height), size),
            }
        }
        if let Backend::Hardware(hw) = &decoder.backend {
            eprintln!(
                "encoder H.264: 8 timely frames; {} VT sessions",
                hw.session_creations
            );
            assert_eq!(hw.session_creations, 2);
        }
        decoder.push_packet(&second[0], 21).unwrap(); // leave a pending output
        assert!(decoder.push_packet(b"garbage", 22).is_err()); // leave a dropped token
        decoder.reset_for_keyframe().unwrap();
        assert!(decoder.poll_owned_frame().unwrap().is_none());
        assert_eq!(decoder.poll_dropped_token(), None);
        assert_eq!(
            decoder.push_packet(&second[1], 23),
            Err(DecodeError::NeedKeyframe)
        );
        assert!(terminal(&mut decoder, 23).is_none());
        assert!(decoder.push_packet(b"\0\0\x01\x65\x80", 24).is_err());
        assert!(terminal(&mut decoder, 24).is_none());
        decoder.push_packet(&first[0], 25).unwrap();
        assert!(terminal(&mut decoder, 25).is_some());
        let cancel = Arc::new(AtomicBool::new(false));
        decoder.set_notification(DecoderNotification::new(cancel.clone()));
        decoder.push_packet(&first[1], 26).unwrap(); // pending when cancellation arrives
        cancel.store(true, Ordering::Release);
        assert!(matches!(
            decoder.poll_owned_frame(),
            Err(DecodeError::Closed)
        ));
        assert_eq!(decoder.push_packet(&first[2], 27), Err(DecodeError::Closed));
        assert_eq!(decoder.poll_dropped_token(), None);
        decoder.set_notification(DecoderNotification::default());
        assert!(decoder.poll_owned_frame().unwrap().is_none());
    }
}

#[test]
#[ignore = "requires VideoToolbox hardware and the external T2a ffmpeg streams"]
fn vt_ffmpeg_streams_switch_resolution() {
    let Some(dir) = std::env::var_os("FARWING_T2A_STREAMS") else {
        eprintln!("skipping: FARWING_T2A_STREAMS is not set");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let size = (160, 128);
    for (codec, name) in [(VideoCodec::H265, "hevc"), (VideoCodec::Av1, "av1")] {
        let data = std::fs::read(dir.join(format!("{name}.stream"))).unwrap();
        let csv = std::fs::read_to_string(dir.join(format!("{name}.csv"))).unwrap();
        let mut decoder =
            PlatformVideoDecoder::open(&config(codec, size, DecoderMode::Hardware)).unwrap();
        assert_eq!(csv.lines().count(), 8);
        let mut end = 0;
        for (i, row) in csv.lines().enumerate() {
            let fields: Vec<_> = row.split(',').collect();
            let len: usize = fields[0].parse().unwrap();
            let pos: usize = fields[1].parse().unwrap();
            assert_eq!(pos, end);
            end = pos + len;
            let token = i as i64;
            decoder.push_packet(&data[pos..end], token).unwrap();
            let Some(PlatformDecodedFrame::Gpu(frame)) = terminal(&mut decoder, token) else {
                panic!("{name} packet {i} produced no GPU frame");
            };
            assert_eq!(
                (frame.width(), frame.height()),
                if i < 4 { size } else { (192, 144) }
            );
            assert_eq!(frame.pixel_format().to_be_bytes(), *b"420v");
        }
        assert_eq!(end, data.len());
        let Backend::Hardware(hw) = &decoder.backend else {
            unreachable!()
        };
        eprintln!(
            "ffmpeg {name}: 8 timely frames, 160x128 → 192x144; {} VT sessions",
            hw.session_creations
        );
        assert_eq!(hw.session_creations, 2);
    }
}

#[test]
#[ignore = "requires VideoToolbox hardware and an x265 open-GOP stream with RASL"]
fn vt_hevc_open_gop_rasl_and_reordering() {
    let Some(dir) = std::env::var_os("FARWING_T2B_STREAMS") else {
        eprintln!("skipping: FARWING_T2B_STREAMS is not set");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let bytes = std::fs::read(dir.join("hevc-open-gop.stream")).unwrap();
    let csv = std::fs::read_to_string(dir.join("hevc-open-gop.csv")).unwrap();
    let packets: Vec<_> = csv
        .lines()
        .map(|row| {
            let mut fields = row.split(',');
            let len: usize = fields.next().unwrap().parse().unwrap();
            let pos: usize = fields.next().unwrap().parse().unwrap();
            let au = &bytes[pos..pos + len];
            let kind = annex_b_units(au)
                .into_iter()
                .map(|nal| (nal[0] >> 1) & 63)
                .find(|&kind| kind <= 31)
                .unwrap();
            (kind, au)
        })
        .collect();
    let cra = packets.iter().position(|(kind, _)| *kind == 21).unwrap();
    let next_cra = cra
        + 1
        + packets[cra + 1..]
            .iter()
            .position(|(kind, _)| *kind == 21)
            .unwrap();
    for start in [0, cra] {
        let mut decoder = PlatformVideoDecoder::open(&config(
            VideoCodec::H265,
            (160, 128),
            DecoderMode::Hardware,
        ))
        .unwrap();
        let mut dropped = 0;
        for (i, &(kind, au)) in packets.iter().enumerate().skip(start) {
            let token = i as i64;
            decoder.push_packet(au, token).unwrap();
            let skip_rasl = start == cra && i < next_cra && matches!(kind, 8 | 9);
            assert_eq!(
                terminal(&mut decoder, token).is_none(),
                skip_rasl,
                "start={start}, packet={i}, nal={kind}"
            );
            dropped += usize::from(skip_rasl);
        }
        eprintln!(
            "x265 open GOP from packet {start}: {} timely frames; {dropped} associated RASL dropped",
            packets.len() - start - dropped
        );
        assert_eq!(dropped > 0, start == cra);
    }
}
