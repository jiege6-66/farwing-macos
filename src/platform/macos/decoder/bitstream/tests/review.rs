use super::super::BitstreamError;
use super::*;
use crate::media::decode_api::DecodeError;

// Two FFmpeg/libx264 colour streams (32x32 with SPS/PPS ID 0 and 48x32
// with ID 1), two frames each, AUDs on and encoder SEI stripped. 137 bytes.
const AVC_SWITCH: &[u8] =
    include_bytes!("../../../../../media/decoder/fixtures/bitstream/avc-resolution-switch.annexb");
const AVC_AU_OFFSETS: [usize; 5] = [0, 53, 67, 122, 137];

fn avc_au(index: usize) -> &'static [u8] {
    &AVC_SWITCH[AVC_AU_OFFSETS[index]..AVC_AU_OFFSETS[index + 1]]
}

fn annex(nals: impl IntoIterator<Item = impl AsRef<[u8]>>) -> Vec<u8> {
    let mut data = Vec::new();
    for nal in nals {
        data.extend_from_slice(&[0, 0, 0, 1]);
        data.extend_from_slice(nal.as_ref());
    }
    data
}

fn nal(input: &[u8], mask: u8, header: u8) -> &[u8] {
    video_format::annex_b_units(input)
        .into_iter()
        .find(|n| n[0] & mask == header)
        .unwrap()
}

fn frame_only(codec: VideoCodec, packet: &[u8]) -> Vec<u8> {
    annex(
        video_format::annex_b_units(packet)
            .into_iter()
            .filter(|n| !is_parameter_set(codec, n)),
    )
}

fn follow_up(stream: &mut Bitstream, codec: VideoCodec, input: &[u8], before: &Configuration) {
    let sample = stream.prepare(input).unwrap();
    assert!(sample.has_picture && !sample.configuration_changed);
    round_trip(codec, input, &sample);
    assert_eq!(stream.configuration(), Some(before));
}

#[test]
fn checked_in_avc_resolution_switch() {
    assert_eq!(AVC_SWITCH.len(), AVC_AU_OFFSETS[4]);
    let codec = VideoCodec::H264;
    let mut stream = Bitstream::new(codec, &[]).unwrap();
    let mut previous = None;
    for i in 0..4 {
        let packet = avc_au(i);
        let sample = stream.prepare(packet).unwrap();
        let current = stream.configuration().unwrap();
        let format = current.format();
        assert!(sample.has_picture);
        assert_eq!(sample.is_keyframe, i % 2 == 0);
        assert_eq!(
            sample.configuration_changed,
            previous.as_ref() != Some(current)
        );
        assert_eq!(sample.configuration_changed, i % 2 == 0);
        assert_eq!(
            (format.visible_width, format.visible_height),
            (if i < 2 { 32 } else { 48 }, 32)
        );
        round_trip(codec, packet, &sample);
        previous = Some(current.clone());
    }
}

#[test]
fn truncated_inactive_avc_pps_leaves_both_valid_chains_usable() {
    let codec = VideoCodec::H264;
    let mut stream = Bitstream::new(codec, &[]).unwrap();
    stream.prepare(avc_au(2)).unwrap();
    let second = stream.configuration().unwrap().clone();
    stream.prepare(avc_au(0)).unwrap();
    let first = stream.configuration().unwrap().clone();
    let pps = nal(avc_au(2), 31, 8);
    // The shortest prefix still exposing PPS ID 1 and SPS ID 1, plus a
    // truncation dropping only rbsp_trailing_bits, are both invalid.
    for len in [3, pps.len() - 1] {
        let rejected = stream.prepare(&annex([&pps[..len]])).is_err();
        assert_eq!(stream.configuration(), Some(&first));
        follow_up(&mut stream, codec, avc_au(1), &first);
        let sample = stream.prepare(avc_au(3)).unwrap();
        round_trip(codec, avc_au(3), &sample);
        assert!(sample.configuration_changed && !sample.is_keyframe);
        assert_eq!(stream.configuration(), Some(&second));
        assert!(rejected);
        stream.prepare(avc_au(0)).unwrap();
    }
}

#[test]
fn avc_pps_waits_for_parent_without_shadowing_the_verified_id() {
    let codec = VideoCodec::H264;
    let mut stream = Bitstream::new(codec, &[]).unwrap();
    stream.prepare(avc_au(2)).unwrap();
    let old = stream.configuration().unwrap().clone();
    let mut pps = nal(avc_au(2), 31, 8).to_vec();
    let mut sps = nal(avc_au(2), 31, 7).to_vec();
    assert_eq!(pps[1] >> 2, 0b010010); // PPS 1 / SPS 1
    assert_eq!(sps[4] >> 5, 0b010); // SPS 1
    pps[1] |= 0b100; // same PPS 1, point to SPS 2 (ue(2) = 011)
    sps[4] |= 0b0010_0000; // SPS 2
    let pending = stream.prepare(&annex([&pps])).unwrap();
    assert!(pending.data.is_empty() && !pending.has_picture && !pending.configuration_changed);
    assert_eq!(stream.configuration(), Some(&old));
    follow_up(&mut stream, codec, avc_au(3), &old);
    assert!(stream.prepare(&annex([&pps[..3]])).is_err());
    follow_up(&mut stream, codec, avc_au(3), &old);
    let validated = stream.prepare(&annex([&sps])).unwrap();
    assert!(validated.data.is_empty() && !validated.has_picture && validated.configuration_changed);
    let new = stream.configuration().unwrap().clone();
    assert_ne!(old, new);
    assert!(matches!(&new, Configuration::AnnexB { sps: a, pps: b, .. } if *a == sps && *b == pps));
    follow_up(&mut stream, codec, avc_au(3), &new);
}

#[test]
fn avc_chroma_switch_validates_the_replacement_pps_not_the_old_one() {
    let codec = VideoCodec::H264;
    // PPS id=0/SPS id=0, transform_8x8_mode + pic_scaling_matrix_present,
    // with 8 or 12 zero pic_scaling_list_present_flags, respectively.
    let pps_420 = [0x68, 0xce, 0x3c, 0xc0, 0x30];
    let pps_444 = [0x68, 0xce, 0x3c, 0xc0, 0x03];
    let input_444 = fixtures::sample(codec, 3, 8, SIZES[0]).unwrap();
    let first = annex([nal(avc_au(0), 31, 7), &pps_420, nal(avc_au(0), 31, 5)]);
    let second = annex([nal(input_444, 31, 7), &pps_444, nal(input_444, 31, 5)]);
    let mut stream = Bitstream::new(codec, &[]).unwrap();
    let sample = stream.prepare(&first).unwrap();
    round_trip(codec, &first, &sample);
    assert_eq!(
        stream.configuration().unwrap().format().chroma_format_idc,
        1
    );
    let sample = stream.prepare(&second).unwrap();
    round_trip(codec, &second, &sample);
    assert!(sample.configuration_changed);
    let after = stream.configuration().unwrap().clone();
    assert_eq!(after.format().chroma_format_idc, 3);
    assert!(matches!(&after, Configuration::AnnexB { pps, .. } if pps.as_ref() == pps_444));
    follow_up(&mut stream, codec, &frame_only(codec, &second), &after);
}

#[test]
fn stack_slice_prefix_preserves_emulation_prevention_in_sample_bytes() {
    let codec = VideoCodec::H264;
    let mut stream = Bitstream::new(codec, &[]).unwrap();
    stream.prepare(avc_au(0)).unwrap();
    let before = stream.configuration().unwrap().clone();
    // Synthetic VCL prefix: a large first_mb_in_slice Exp-Golomb spanning
    // an emulation-prevention byte, then slice_type=0 and PPS ID 0.
    let mut picture = annex([[0x41, 0, 0, 3, 0, 0x80, 0, 0, 0x60]]);
    picture.extend_from_slice(&[0xff; 96]);
    follow_up(&mut stream, codec, &picture, &before);
    follow_up(&mut stream, codec, avc_au(1), &before);
}

#[test]
fn truncated_hevc_vps_does_not_replace_the_verified_chain() {
    let codec = VideoCodec::H265;
    let input = fixtures::sample(codec, 1, 8, SIZES[0]).unwrap();
    let mut stream = Bitstream::new(codec, &[]).unwrap();
    stream.prepare(input).unwrap();
    let before = stream.configuration().unwrap().clone();
    let vps = nal(input, 0x7e, 32 << 1);
    assert_eq!(vps.len(), 24);
    let next = frame_only(codec, input);
    for cut in [21, 23] {
        let result = stream.prepare(&annex([&vps[..cut]]));
        assert_eq!(stream.configuration(), Some(&before));
        follow_up(&mut stream, codec, &next, &before);
        assert!(result.is_err());
    }
}

#[test]
fn sps_after_vcl_cannot_change_the_current_chain_retroactively() {
    for (codec, mask, header) in [(VideoCodec::H264, 31, 7), (VideoCodec::H265, 0x7e, 33 << 1)] {
        let current = fixtures::sample(codec, 1, 8, SIZES[0]).unwrap();
        let later = fixtures::sample(codec, 1, 8, SIZES[1]).unwrap();
        let mut stream = Bitstream::new(codec, &[]).unwrap();
        let picture = fixtures::first_picture(codec, current);
        let good = stream.prepare(&picture).unwrap();
        let before = stream.configuration().unwrap().clone();
        let mut complete = picture.clone();
        // A byte-identical reannouncement after VCL is harmless and omitted from the sample.
        complete.extend(annex([nal(current, mask, header)]));
        let sample = stream.prepare(&complete).unwrap();
        assert_eq!(sample.data, good.data);
        assert_eq!(stream.configuration(), Some(&before));
        complete.extend(annex([nal(later, mask, header)]));
        let result = stream.prepare(&complete);
        assert!(result.is_err());
        assert_eq!(stream.configuration(), Some(&before));
        follow_up(&mut stream, codec, &frame_only(codec, &picture), &before);
    }
}

#[test]
fn late_avc_parameter_sets_for_another_id_are_available_to_the_next_au() {
    let codec = VideoCodec::H264;
    let mut stream = Bitstream::new(codec, &[]).unwrap();
    let mut input = avc_au(0).to_vec();
    input.extend(annex([nal(avc_au(2), 31, 7), nal(avc_au(2), 31, 8)]));
    let first = stream.prepare(&input).unwrap();
    round_trip(codec, avc_au(0), &first);
    assert_eq!(stream.configuration().unwrap().format().visible_width, 32);
    let next = stream.prepare(&frame_only(codec, avc_au(2))).unwrap();
    round_trip(codec, avc_au(2), &next);
    assert!(next.configuration_changed && next.is_keyframe);
    let second = stream.configuration().unwrap().clone();
    assert_eq!(second.format().visible_width, 48);
    follow_up(&mut stream, codec, avc_au(3), &second);
}

#[test]
fn avc_and_hevc_accept_header_only_end_nals_after_picture() {
    for (codec, headers) in [
        (VideoCodec::H264, [&[10][..], &[11][..]]),
        (VideoCodec::H265, [&[36 << 1, 1][..], &[37 << 1, 1][..]]),
    ] {
        let input =
            fixtures::first_picture(codec, fixtures::sample(codec, 1, 8, SIZES[0]).unwrap());
        let mut stream = Bitstream::new(codec, &[]).unwrap();
        stream.prepare(&input).unwrap();
        let before = stream.configuration().unwrap().clone();
        let mut end = input.clone();
        end.extend(annex(headers));
        follow_up(&mut stream, codec, &end, &before);
        follow_up(&mut stream, codec, &frame_only(codec, &input), &before);
    }
}

#[test]
fn avc_sps_extension_is_not_silently_treated_as_sample_data() {
    let codec = VideoCodec::H264;
    // seq_parameter_set_id=0, aux_format_idc=0, additional_extension_flag=0, trailing bits.
    let extension = annex([[0x6d, 0xd0]]);
    let mut stream = Bitstream::new(codec, &[]).unwrap();
    stream.prepare(avc_au(0)).unwrap();
    let before = stream.configuration().unwrap().clone();
    let input = [avc_au(1), extension.as_slice()].concat();
    assert!(stream.prepare(&input).is_err());
    assert_eq!(stream.configuration(), Some(&before));
    follow_up(&mut stream, codec, avc_au(1), &before);
    assert!(matches!(
        Bitstream::new(codec, &extension),
        Err(BitstreamError::Unsupported(_))
    ));
}

fn av1_parts() -> (&'static [u8], &'static [u8]) {
    let input = fixtures::sample(VideoCodec::Av1, 1, 8, SIZES[0]).unwrap();
    let mut sequence = None;
    let mut frame = None;
    for obu in Units::new(input).unwrap().map(Result::unwrap) {
        match obu.kind {
            1 => sequence = Some(obu.bytes),
            6 => frame = Some(obu.bytes),
            _ => {}
        }
    }
    (sequence.unwrap(), frame.unwrap())
}

fn frame_prefix(frame: &[u8], prefix: u8) -> Vec<u8> {
    let payload_len = Units::new(frame)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .payload
        .len();
    let mut data = frame.to_vec();
    let index = data.len() - payload_len;
    data[index] = data[index] & 0x0f | prefix;
    data
}

#[test]
fn av1_hidden_key_is_not_a_sync_sample() {
    let codec = VideoCodec::Av1;
    let (seq, frame) = av1_parts();
    let mut stream = Bitstream::new(codec, seq).unwrap();
    let before = stream.configuration().unwrap().clone();
    let hidden = frame_prefix(frame, 0); // KEY_FRAME with show_frame=0
    let input = [seq, &hidden].concat();
    let sample = stream.prepare(&input).unwrap();
    assert_eq!(sample.data, input);
    assert!(!sample.configuration_changed && sample.has_picture && !sample.is_keyframe);
    assert_eq!(stream.configuration(), Some(&before));
    let next = stream.prepare(&[seq, frame].concat()).unwrap();
    assert!(next.is_keyframe);
    round_trip(codec, &[seq, frame].concat(), &next);
    assert_eq!(stream.configuration(), Some(&before));
}

#[test]
fn av1_only_first_frame_in_a_temporal_unit_decides_sync() {
    let codec = VideoCodec::Av1;
    let (seq, frame) = av1_parts();
    let mut stream = Bitstream::new(codec, seq).unwrap();
    let before = stream.configuration().unwrap().clone();
    let inter = frame_prefix(frame, 0x30); // INTER_FRAME with show_frame=1
    let mut enhancement = frame.to_vec();
    enhancement[0] |= 4;
    enhancement.insert(1, 1 << 3); // spatial_id = 1
    let input = [seq, &inter, &enhancement].concat();
    let sample = stream.prepare(&input).unwrap();
    assert_eq!(sample.data, input);
    assert!(!sample.configuration_changed && sample.has_picture && !sample.is_keyframe);
    assert_eq!(stream.configuration(), Some(&before));
    follow_up(&mut stream, codec, frame, &before);
}

#[test]
fn av1_handles_show_existing_and_implicit_shown_still_picture() {
    let codec = VideoCodec::Av1;
    let (seq, frame) = av1_parts();
    let mut stream = Bitstream::new(codec, seq).unwrap();
    let before = stream.configuration().unwrap().clone();
    // OBU_FRAME_HEADER, show_existing_frame=1, slot=0, trailing bit.
    let show_existing = [0x1a, 1, 0x88];
    let input = [seq, &show_existing, frame].concat();
    let sample = stream.prepare(&input).unwrap();
    assert_eq!(sample.data, input);
    assert!(!sample.is_keyframe && !sample.configuration_changed);
    assert_eq!(stream.configuration(), Some(&before));
    follow_up(&mut stream, codec, frame, &before);

    // Reduced still picture: the frame payload does not signal the prefix;
    // the first frame is implicitly a shown key frame.
    let reduced = [0x0a, 5, 0x18, 0, 0, 0x02, 0x20];
    let still = [0x32, 1, 0x80];
    let input = [reduced.as_slice(), &still].concat();
    let sample = stream.prepare(&input).unwrap();
    assert_eq!(sample.data, input);
    assert!(sample.is_keyframe && sample.configuration_changed);
    let after = stream.configuration().unwrap().clone();
    assert!(matches!(
        after,
        Configuration::Av1 {
            reduced_still_picture_header: true,
            ..
        }
    ));
    let next = stream.prepare(&still).unwrap();
    assert_eq!(next.data, still);
    assert!(next.has_picture && !next.is_keyframe && !next.configuration_changed);
    assert_eq!(stream.configuration(), Some(&after));
}

#[test]
fn av1_rejects_a_second_temporal_unit_in_the_same_packet() {
    let codec = VideoCodec::Av1;
    let (seq, frame) = av1_parts();
    let mut stream = Bitstream::new(codec, seq).unwrap();
    let before = stream.configuration().unwrap().clone();
    let delimiter = &[0x12, 0];
    let good = [delimiter, seq, frame].concat();
    let bad = [good.as_slice(), delimiter, frame].concat();
    assert!(stream.prepare(&bad).is_err());
    assert!(
        stream
            .prepare(&[delimiter.as_slice(), delimiter].concat())
            .is_err()
    );
    assert_eq!(stream.configuration(), Some(&before));
    follow_up(&mut stream, codec, &good, &before);
}

#[test]
fn av1_split_frame_requires_matching_tile_group_layer_ids() {
    let codec = VideoCodec::Av1;
    let (seq, _) = av1_parts();
    let mut stream = Bitstream::new(codec, seq).unwrap();
    let before = stream.configuration().unwrap().clone();
    // Frame-header/tile-group OBUs with extension IDs (temporal=1, spatial=1).
    let header = [0x1e, 0x28, 1, 0x10];
    let tile = [0x26, 0x28, 1, 0x80];
    for id in [0x08, 0x20] {
        let mut mismatch = tile;
        mismatch[1] = id;
        assert!(stream.prepare(&[header, mismatch].concat()).is_err());
        assert_eq!(stream.configuration(), Some(&before));
        follow_up(&mut stream, codec, &[header, tile].concat(), &before);
    }
}

#[test]
fn conversion_errors_map_to_decode_error_without_string_matching() {
    let codec = VideoCodec::H264;
    let mut stream = Bitstream::new(codec, &[]).unwrap();
    assert!(matches!(
        stream.prepare(avc_au(1)),
        Err(BitstreamError::MissingParameters(_))
    ));
    assert!(stream.configuration().is_none());
    let key = stream.prepare(avc_au(0)).unwrap();
    round_trip(codec, avc_au(0), &key);
    let before = stream.configuration().unwrap().clone();
    for (packet, expected) in [
        (avc_au(3), DecodeError::NeedKeyframe),
        (&[0, 0, 1, 0x65], DecodeError::InvalidInput),
        (&[0, 0, 1, 0x6d, 0xd0], DecodeError::Unsupported),
    ] {
        let Err(error) = stream.prepare(packet) else {
            panic!("expected {expected:?}")
        };
        assert_eq!(DecodeError::from(error), expected);
        assert_eq!(stream.configuration(), Some(&before));
        follow_up(&mut stream, codec, avc_au(1), &before);
    }
    let (seq, frame) = av1_parts();
    let mut av1_stream = Bitstream::new(VideoCodec::Av1, &[]).unwrap();
    assert!(matches!(
        av1_stream.prepare(frame),
        Err(BitstreamError::MissingParameters(_))
    ));
    assert!(av1_stream.configuration().is_none());
    let key = [seq, frame].concat();
    let sample = av1_stream.prepare(&key).unwrap();
    assert!(sample.is_keyframe && sample.hevc_vcl_nal_type.is_none());
    assert_eq!(sample.data, key);
    let before = av1_stream.configuration().unwrap().clone();
    follow_up(&mut av1_stream, VideoCodec::Av1, frame, &before);
}

#[test]
fn hevc_reserved_irap_types_are_rejected_without_affecting_next_frame() {
    let codec = VideoCodec::H265;
    let input = fixtures::first_picture(codec, fixtures::sample(codec, 1, 8, SIZES[0]).unwrap());
    let mut stream = Bitstream::new(codec, &[]).unwrap();
    stream.prepare(&input).unwrap();
    let before = stream.configuration().unwrap().clone();
    let vcl = video_format::annex_b_units(&input)
        .into_iter()
        .find(|n| matches!(n[0] >> 1, 19 | 20))
        .unwrap();
    for kind in 16..=21 {
        let mut irap = vcl.to_vec();
        irap[0] = kind << 1;
        let packet = annex([irap]);
        let sample = stream.prepare(&packet).unwrap();
        assert!(sample.is_keyframe && !sample.configuration_changed);
        assert_eq!(sample.hevc_vcl_nal_type, Some(kind));
        round_trip(codec, &packet, &sample);
        assert_eq!(stream.configuration(), Some(&before));
    }
    for kind in [22, 23] {
        let mut reserved = vcl.to_vec();
        reserved[0] = kind << 1;
        assert!(stream.prepare(&annex([reserved])).is_err());
        assert_eq!(stream.configuration(), Some(&before));
        follow_up(&mut stream, codec, &frame_only(codec, &input), &before);
    }
}
