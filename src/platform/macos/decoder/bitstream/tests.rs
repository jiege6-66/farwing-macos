use super::{Bitstream, Configuration, Sample};
use crate::media::{VideoCodec, decoder::fixtures, video_format};
use farwing_codec::syntax::av1::obu::Units;

mod review;

const SIZES: [(u32, u32); 4] = [(1280, 720), (1920, 1080), (2560, 1440), (3840, 2160)];
const CODECS: [VideoCodec; 3] = [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1];

fn is_parameter_set(codec: VideoCodec, nal: &[u8]) -> bool {
    match codec {
        VideoCodec::H264 => matches!(nal[0] & 31, 7 | 8),
        VideoCodec::H265 => matches!((nal[0] >> 1) & 63, 32..=34),
        VideoCodec::Av1 => unreachable!(),
    }
}

fn round_trip(codec: VideoCodec, input: &[u8], sample: &Sample) {
    if codec == VideoCodec::Av1 {
        let expected: Vec<_> = Units::new(input)
            .unwrap()
            .map(Result::unwrap)
            .filter(|obu| obu.kind != 2)
            .flat_map(|obu| obu.bytes.iter().copied())
            .collect();
        assert_eq!(sample.data, expected);
    } else {
        let mut data = sample.data.as_slice();
        for nal in video_format::annex_b_units(input)
            .into_iter()
            .filter(|nal| !is_parameter_set(codec, nal))
        {
            let size = u32::from_be_bytes(data[..4].try_into().unwrap()) as usize;
            assert_eq!(nal.len(), size);
            assert_eq!(&data[4..4 + size], nal);
            data = &data[4 + size..];
        }
        assert!(data.is_empty());
    }
}

#[test]
fn converts_every_matrix_fixture() {
    let mut count = 0;
    for codec in CODECS {
        for chroma in [1, 3] {
            for depth in [8, 10] {
                for (i, size) in SIZES.into_iter().enumerate() {
                    let Some(input) = fixtures::sample(codec, chroma, depth, size) else {
                        continue;
                    };
                    count += 1;
                    let picture = fixtures::first_picture(codec, input);
                    let mut stream = Bitstream::new(codec, &[]).unwrap();
                    let sample = stream.prepare(&picture).unwrap();
                    assert!(
                        sample.is_keyframe && sample.has_picture && sample.configuration_changed
                    );
                    round_trip(codec, &picture, &sample);
                    let config = stream.configuration().unwrap();
                    let format = config.format();
                    assert_eq!(
                        format,
                        video_format::parse_stream_format(codec, input).unwrap()
                    );
                    assert_eq!((format.visible_width, format.visible_height), size);
                    assert_eq!(format.chroma_format_idc, chroma);
                    assert_eq!(
                        (format.bit_depth_luma, format.bit_depth_chroma),
                        (depth, depth)
                    );
                    let mut extra = Vec::new();
                    let mut picture_only = Vec::new();
                    match config {
                        Configuration::Av1 { av1c, .. } => {
                            // Independently obtained by remuxing the 16 fixtures into MP4 with ffmpeg.
                            let (profile_level, tier_chroma_depth) = match chroma {
                                1 => ([0x09, 0x0d, 0x0e, 0x11][i], [0x8c, 0x0c, 0x0c, 0x0c][i]),
                                _ => ([0x28, 0x29, 0x2c, 0x2d][i], 0),
                            };
                            let depth_bit = if depth == 10 { 0x40 } else { 0 };
                            assert_eq!(
                                av1c[..4],
                                [0x81, profile_level, tier_chroma_depth | depth_bit, 0]
                            );
                            for obu in Units::new(input).unwrap().map(Result::unwrap) {
                                if obu.kind == 1 {
                                    assert_eq!(&av1c[4..], obu.bytes);
                                    extra.extend_from_slice(obu.bytes);
                                } else {
                                    picture_only.extend_from_slice(obu.bytes);
                                }
                            }
                        }
                        Configuration::AnnexB { vps, sps, pps, .. } => {
                            let expected: Vec<&[u8]> =
                                vps.iter().chain([sps, pps]).map(AsRef::as_ref).collect();
                            let units = video_format::annex_b_units(&picture);
                            assert_eq!(
                                expected,
                                units
                                    .iter()
                                    .copied()
                                    .filter(|nal| is_parameter_set(codec, nal))
                                    .collect::<Vec<_>>()
                            );
                            for nal in units {
                                let out = if is_parameter_set(codec, nal) {
                                    &mut extra
                                } else {
                                    &mut picture_only
                                };
                                // Exercise 3-byte prefixes, also allowing leading/trailing zero bytes.
                                out.extend_from_slice(&[0, 0, 1]);
                                out.extend_from_slice(nal);
                            }
                            extra.insert(0, 0);
                            extra.extend_from_slice(&[0, 0]);
                        }
                    }

                    let mut seeded = Bitstream::new(codec, &extra).unwrap();
                    assert_eq!(seeded.configuration(), Some(config));
                    let bare = seeded.prepare(&picture_only).unwrap();
                    assert!(bare.has_picture && !bare.configuration_changed);
                    assert_eq!(bare.is_keyframe, codec != VideoCodec::Av1);
                    round_trip(codec, &picture_only, &bare);
                    assert!(!seeded.prepare(&picture).unwrap().configuration_changed);
                }
            }
        }
    }
    assert_eq!(count, 40);
}

fn unsized_obu(bytes: &[u8], payload: &[u8]) -> Vec<u8> {
    let header_len = 1 + usize::from(bytes[0] & 4 != 0);
    let mut raw = bytes[..header_len].to_vec();
    raw[0] &= !2;
    raw.extend_from_slice(payload);
    raw
}

#[test]
fn normalizes_an_av1_obu_without_a_size_at_end_of_packet() {
    let input = fixtures::sample(VideoCodec::Av1, 1, 8, SIZES[0]).unwrap();
    let units: Vec<_> = Units::new(input).unwrap().map(Result::unwrap).collect();
    let sequence = units.iter().find(|obu| obu.kind == 1).unwrap();
    let mut stream = Bitstream::new(
        VideoCodec::Av1,
        &unsized_obu(sequence.bytes, sequence.payload),
    )
    .unwrap();
    let Some(Configuration::Av1 { av1c, .. }) = stream.configuration() else {
        panic!("missing av1C")
    };
    assert_eq!(&av1c[4..], sequence.bytes);
    assert_eq!(units.last().unwrap().kind, 6);
    let last = units.last().unwrap();
    let mut packet = input[..input.len() - last.bytes.len()].to_vec();
    packet.extend_from_slice(&unsized_obu(last.bytes, last.payload));
    let converted = stream.prepare(&packet).unwrap();
    assert!(converted.is_keyframe && !converted.configuration_changed);
    round_trip(VideoCodec::Av1, input, &converted);
}

#[test]
fn reduced_av1_sequence_records_twelve_bit_monochrome() {
    // AV1 5.5: profile 2, still/reduced header, level 0, 1x1, no optional
    // filters, 12-bit monochrome, no colour description or film grain.
    let sequence = [0x0a, 5, 0x58, 0, 0, 0x38, 0x80];
    let stream = Bitstream::new(VideoCodec::Av1, &sequence).unwrap();
    let Some(Configuration::Av1 { av1c, format, .. }) = stream.configuration() else {
        panic!("missing av1C")
    };
    assert_eq!(av1c[..4], [0x81, 0x40, 0x7c, 0]);
    assert_eq!(&av1c[4..], &sequence);
    assert_eq!((format.visible_width, format.visible_height), (1, 1));
    assert_eq!((format.chroma_format_idc, format.bit_depth_luma), (0, 12));
}

#[test]
fn av1c_preserves_chroma_sample_position() {
    // Reduced 8-bit 4:2:0 sequence with chroma_sample_position = 2 (colocated).
    let sequence = [0x0a, 5, 0x18, 0, 0, 0x02, 0x20];
    let stream = Bitstream::new(VideoCodec::Av1, &sequence).unwrap();
    let Some(Configuration::Av1 { av1c, .. }) = stream.configuration() else {
        panic!("missing av1C")
    };
    assert_eq!(av1c[..4], [0x81, 0, 0x0e, 0]);
}

#[test]
fn annex_b_applies_a_standalone_sps_update_without_discarding_pps() {
    for (codec, kind) in [(VideoCodec::H264, 7), (VideoCodec::H265, 33)] {
        let input = fixtures::sample(codec, 1, 8, SIZES[0]).unwrap();
        let mut stream = Bitstream::new(codec, &[]).unwrap();
        stream.prepare(input).unwrap();
        let Some(Configuration::AnnexB { vps, pps, .. }) = stream.configuration().cloned() else {
            panic!("missing Annex-B configuration")
        };
        let next = fixtures::sample(codec, 1, 8, SIZES[1]).unwrap();
        let sps = video_format::annex_b_units(next)
            .into_iter()
            .find(|nal| {
                if codec == VideoCodec::H264 {
                    nal[0] & 31 == kind
                } else {
                    (nal[0] >> 1) & 63 == kind
                }
            })
            .unwrap();
        let mut update = vec![0, 0, 1];
        update.extend_from_slice(sps);
        let sample = stream.prepare(&update).unwrap();
        assert!(
            sample.data.is_empty()
                && !sample.has_picture
                && !sample.is_keyframe
                && sample.configuration_changed
        );
        let Some(Configuration::AnnexB {
            vps: new_vps,
            sps: new_sps,
            pps: new_pps,
            format,
            ..
        }) = stream.configuration()
        else {
            panic!("missing Annex-B configuration")
        };
        assert_eq!(new_vps, &vps);
        assert_eq!(new_sps, sps);
        assert_eq!(new_pps, &pps);
        assert_eq!((format.visible_width, format.visible_height), SIZES[1]);
        assert!(!stream.prepare(&update).unwrap().configuration_changed);
    }
}

#[test]
fn invalid_input_returns_errors_without_updating_configuration() {
    for codec in CODECS {
        let mut stream = Bitstream::new(codec, &[]).unwrap();
        assert!(stream.prepare(&[]).is_err());
        assert!(stream.prepare(b"junk").is_err());
        let input = fixtures::sample(codec, 1, 8, SIZES[0]).unwrap();
        stream
            .prepare(&fixtures::first_picture(codec, input))
            .unwrap();
        let before = stream.configuration().cloned();
        let malformed: &[&[u8]] = match codec {
            VideoCodec::H264 => &[
                &[0, 0, 1],
                &[0, 0, 1, 0x65],
                &[0, 0, 1, 0x65, 0xb8],
                &[0, 0, 1, 0x67, 0x64],
                &[0, 0, 1, 0x68, 0xe0],
                &[0, 0, 1, 0, 0, 1, 9, 0x10],
            ],
            VideoCodec::H265 => &[
                &[0, 0, 0, 1],
                &[0, 0, 1, 0x26, 1],
                &[0, 0, 1, 0x26, 1, 0xe0],
                &[0, 0, 1, 0x42, 1, 0x30],
                &[0, 0, 1, 0x44, 1, 0x10],
            ],
            VideoCodec::Av1 => &[
                &[0x12],
                &[0x0a, 8, 0],
                &[0x0e],
                &[0x32, 0],
                &[0x1a, 1, 0x10], // frame header without tile data
                &[0x22, 1, 0x80], // tile group without a frame header
                &[0x12, 1, 0],
                &[0x06, 7],
            ],
        };
        for &data in malformed {
            assert!(stream.prepare(data).is_err(), "{codec:?} {data:?}");
            assert_eq!(stream.configuration(), before.as_ref());
        }
        if codec != VideoCodec::Av1 {
            let mut replacement = fixtures::sample(codec, 1, 8, SIZES[1]).unwrap().to_vec();
            replacement.extend_from_slice(&[0, 0, 1]);
            assert!(stream.prepare(&replacement).is_err());
            assert_eq!(stream.configuration(), before.as_ref());
            assert!(!stream.prepare(input).unwrap().configuration_changed);
        }
    }
}

#[test]
fn fixed_seed_noise_does_not_panic() {
    let mut seed = 0xa24b_31d7u32;
    let mut byte = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed as u8
    };
    for _ in 0..6000 {
        let mut packet: Vec<_> = (0..byte() % 96).map(|_| byte()).collect();
        for codec in CODECS {
            if let Ok(mut stream) = Bitstream::new(codec, &[]) {
                let _ = stream.prepare(&packet);
            }
        }
        packet.splice(..0, [0, 0, 1, [0x67, 0x68, 0x65][usize::from(byte() % 3)]]);
        let _ = Bitstream::new(VideoCodec::H264, &[])
            .unwrap()
            .prepare(&packet);
        packet[3] = [0x40, 0x42, 0x44, 0x26][usize::from(byte() % 4)];
        packet.insert(4, 1);
        let _ = Bitstream::new(VideoCodec::H265, &[])
            .unwrap()
            .prepare(&packet);
    }
}

#[test]
#[ignore = "requires external ffmpeg-generated streams and ffprobe packet offsets"]
fn generated_multiframe_resolution_switch() {
    let dir = std::path::PathBuf::from(std::env::var_os("FARWING_T2A_STREAMS").unwrap());
    for (codec, name) in CODECS.into_iter().zip(["h264", "hevc", "av1"]) {
        let data = std::fs::read(dir.join(format!("{name}.stream"))).unwrap();
        let mut stream = Bitstream::new(codec, &[]).unwrap();
        let csv = std::fs::read_to_string(dir.join(format!("{name}.csv"))).unwrap();
        let mut end = 0;
        assert_eq!(csv.lines().count(), 8);
        for (i, row) in csv.lines().enumerate() {
            let mut fields = row.split(',');
            let size: usize = fields.next().unwrap().parse().unwrap();
            let pos: usize = fields.next().unwrap().parse().unwrap();
            let key = fields.next().unwrap().starts_with('K');
            assert_eq!(end, pos);
            end = pos + size;
            let packet = &data[pos..end];
            let sample = stream.prepare(packet).unwrap();
            let format = stream.configuration().unwrap().format();
            assert!(sample.has_picture, "{name} packet {i}");
            assert_eq!(sample.is_keyframe, key, "{name} packet {i}");
            assert_eq!(
                sample.configuration_changed,
                i % 4 == 0,
                "{name} packet {i}"
            );
            assert_eq!(
                (format.visible_width, format.visible_height),
                if i < 4 { (160, 128) } else { (192, 144) }
            );
            round_trip(codec, packet, &sample);
        }
        assert_eq!(end, data.len());
        println!(
            "{name}: 8 access units; {data_len} bytes; 160x128 → 192x144",
            data_len = data.len()
        );
    }
}
