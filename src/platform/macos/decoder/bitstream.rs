//! Prepares one Farwing access unit at a time for CoreMedia. No FFI or session state.

use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use farwing_codec::syntax::{
    av1::{
        Chroma, Sequence,
        obu::{Obu, Units},
    },
    h264::{pps::Pps, sps::Sps},
    hevc::{pps::PicParameterSet, sps::SeqParameterSet},
};

use crate::media::{
    VideoCodec, av1,
    video_format::{self, BitReader, VideoFormatSignature, remove_emulation_prevention},
};

/// Compare this with the configuration installed in the decoder, not just
/// with the previous packet's configuration (parameter-only packets can update it).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Configuration {
    AnnexB {
        /// Raw NALs, including NAL headers, but without start codes or lengths.
        vps: Option<Vec<u8>>,
        sps: Vec<u8>,
        pps: Vec<u8>,
        format: VideoFormatSignature,
    },
    Av1 {
        /// AV1CodecConfigurationRecord payload, without the atom size/type.
        av1c: Vec<u8>,
        format: VideoFormatSignature,
        reduced_still_picture_header: bool,
    },
}

impl Configuration {
    pub(crate) fn format(&self) -> VideoFormatSignature {
        match self {
            Self::AnnexB { format, .. } | Self::Av1 { format, .. } => *format,
        }
    }
}

pub(crate) struct Sample {
    /// Four-byte big-endian NAL lengths, or sized AV1 OBUs.
    pub(crate) data: Vec<u8>,
    pub(crate) has_picture: bool,
    /// AVC IDR, HEVC IRAP, or an AV1 key frame with an in-band sequence header.
    pub(crate) is_keyframe: bool,
    pub(crate) configuration_changed: bool,
}

struct Parameter {
    nal: Vec<u8>,
    parent_id: u32,
    format: Option<VideoFormatSignature>,
}

type Parameters = BTreeMap<(u8, u32), Parameter>;

pub(crate) struct Bitstream {
    codec: VideoCodec,
    parameters: Parameters,
    active_pps: Option<u32>,
    configuration: Option<Configuration>,
}

impl Bitstream {
    /// Farwing extradata is empty, Annex-B parameter sets, or AV1 sequence OBUs,
    /// not a container's avcC/hvcC/av1C record.
    pub(crate) fn new(codec: VideoCodec, extra_data: &[u8]) -> Result<Self> {
        let mut stream = Self {
            codec,
            parameters: Parameters::new(),
            active_pps: None,
            configuration: None,
        };
        if !extra_data.is_empty() {
            ensure!(
                !stream.prepare(extra_data)?.has_picture,
                "picture in codec extradata"
            );
        }
        Ok(stream)
    }

    pub(crate) fn configuration(&self) -> Option<&Configuration> {
        self.configuration.as_ref()
    }

    /// Call with an already framed access unit, not an entire elementary stream.
    /// A framing/header error leaves the previous configuration intact. This is
    /// not an entropy decoder; validation of coded picture data belongs to VT.
    pub(crate) fn prepare(&mut self, data: &[u8]) -> Result<Sample> {
        ensure!(!data.is_empty(), "empty access unit");
        if self.codec == VideoCodec::Av1 {
            self.prepare_av1(data)
        } else {
            self.prepare_annex_b(data)
        }
    }

    fn prepare_annex_b(&mut self, data: &[u8]) -> Result<Sample> {
        let hevc = self.codec == VideoCodec::H265;
        let start_code = |data: &[u8]| data.windows(3).position(|w| w == [0, 0, 1]);
        let start = start_code(data).context("missing Annex-B start code")?;
        ensure!(
            data[..start].iter().all(|&v| v == 0),
            "garbage before Annex-B start code"
        );
        let mut remaining = &data[start + 3..];
        let mut updates = Parameters::new();
        let mut sample = Vec::new();
        let mut is_keyframe = false;
        let mut picture_pps = None;
        loop {
            let next = start_code(remaining);
            // A four-byte start code has an extra zero immediately before
            // the three-byte prefix; strip it along with trailing_zero_8bits.
            let nal = &remaining[..next.unwrap_or(remaining.len())];
            let end = nal.iter().rposition(|&v| v != 0).map_or(0, |i| i + 1);
            let nal = &nal[..end];
            let (kind, header_len) = nal_header(hevc, nal).context("invalid or truncated NAL")?;
            if if hevc {
                matches!(kind, 32..=34)
            } else {
                matches!(kind, 7 | 8)
            } {
                let (id, entry) = parameter(hevc, kind, nal, header_len)
                    .context("invalid or truncated parameter set")?;
                updates.insert((kind, id), entry);
            } else {
                if if hevc {
                    kind <= 31
                } else {
                    matches!(kind, 1 | 2 | 5)
                } {
                    let id = slice_pps(hevc, kind, &nal[header_len..])
                        .context("invalid or truncated slice header")?;
                    ensure!(
                        picture_pps.is_none_or(|pps| pps == id),
                        "multiple PPS IDs in one picture"
                    );
                    picture_pps = Some(id);
                    is_keyframe |= if hevc {
                        matches!(kind, 16..=23)
                    } else {
                        kind == 5
                    };
                }
                // Preserve AUD, SEI and other non-parameter-set NALs in order,
                // like the previous generic Annex-B converter.
                sample.extend_from_slice(&u32::try_from(nal.len())?.to_be_bytes());
                sample.extend_from_slice(nal);
            }
            let Some(next) = next else { break };
            remaining = &remaining[next + 3..];
        }

        let candidates = |kind, id| {
            updates
                .get(&(kind, id))
                .or_else(|| self.parameters.get(&(kind, id)))
        };
        let pps_kind = if hevc { 34 } else { 8 };
        let get_chain = |id| {
            let pps = candidates(pps_kind, id)?;
            let sps = candidates(if hevc { 33 } else { 7 }, pps.parent_id)?;
            let vps = if hevc {
                Some(candidates(32, sps.parent_id)?)
            } else {
                None
            };
            Some((vps, sps, pps))
        };
        // Without VCL, preserve the active chain or seed the first complete
        // chain from extradata. A VCL header selects its actual PPS by ID.
        let pps_id = picture_pps.or(self.active_pps).or_else(|| {
            updates
                .keys()
                .chain(self.parameters.keys())
                .find_map(|&(kind, id)| (kind == pps_kind && get_chain(id).is_some()).then_some(id))
        });
        let chain = pps_id.and_then(get_chain);
        ensure!(
            picture_pps.is_none() || chain.is_some(),
            "missing parameter set for picture"
        );
        let mut configuration_changed = false;
        if let Some((vps, sps, pps)) = chain {
            let vps = vps.map(|v| &v.nal);
            let unchanged = matches!(
                &self.configuration,
                Some(Configuration::AnnexB { vps: old_vps, sps: old_sps, pps: old_pps, .. })
                    if old_vps.as_ref() == vps && old_sps == &sps.nal && old_pps == &pps.nal
            );
            if !unchanged {
                let format = sps.format.context("missing SPS format")?;
                if !hevc {
                    let rbsp = remove_emulation_prevention(&pps.nal[1..]);
                    Pps::parse_with_chroma_format(&rbsp, format.chroma_format_idc.into())
                        .context("invalid H.264 PPS")?;
                }
                self.configuration = Some(Configuration::AnnexB {
                    vps: vps.cloned(),
                    sps: sps.nal.clone(),
                    pps: pps.nal.clone(),
                    format,
                });
                configuration_changed = true;
            }
            self.active_pps = pps_id;
        }
        self.parameters.extend(updates);
        Ok(Sample {
            data: sample,
            has_picture: picture_pps.is_some(),
            is_keyframe,
            configuration_changed,
        })
    }

    fn prepare_av1(&mut self, data: &[u8]) -> Result<Sample> {
        let mut sample = Vec::new();
        let mut sequence = None;
        let mut av1c = Vec::new();
        let mut reduced = match self.configuration {
            Some(Configuration::Av1 {
                reduced_still_picture_header,
                ..
            }) => Some(reduced_still_picture_header),
            _ => None,
        };
        let mut has_picture = false;
        let mut is_keyframe = false;
        let mut tile_group_seen = None;
        for obu in Units::with_unsized_last(data)? {
            let obu = obu?;
            match obu.kind {
                1 => {
                    ensure!(!has_picture, "AV1 sequence header after frame");
                    let parsed = Sequence::parse(obu)?;
                    let config = av1_config(parsed, obu);
                    ensure!(
                        sequence.is_none() || av1c == config,
                        "multiple AV1 sequences in access unit"
                    );
                    av1c = config;
                    sequence = Some(parsed);
                    reduced = Some(parsed.reduced_still_picture_header);
                }
                2 => {
                    ensure!(obu.payload.is_empty(), "invalid AV1 temporal delimiter");
                    continue;
                }
                3 | 6 => {
                    ensure!(tile_group_seen != Some(false), "missing AV1 tile group");
                    let reduced = reduced.context("missing AV1 sequence header for picture")?;
                    ensure!(!obu.payload.is_empty(), "empty AV1 frame header");
                    let mut bits = BitReader::new(obu.payload);
                    let show_existing = !reduced && bits.read_bit() == Some(1);
                    let key = if show_existing {
                        ensure!(
                            obu.kind == 3 && bits.read_bits(3).is_some(),
                            "invalid AV1 show_existing_frame"
                        );
                        false
                    } else {
                        reduced || bits.read_bits(2).context("missing AV1 frame type")? == 0
                    };
                    tile_group_seen = (obu.kind == 3 && !show_existing).then_some(false);
                    has_picture = true;
                    is_keyframe |= sequence.is_some() && key;
                }
                4 => {
                    ensure!(
                        tile_group_seen.is_some() && !obu.payload.is_empty(),
                        "unexpected or empty AV1 tile group"
                    );
                    tile_group_seen = Some(true);
                }
                _ => {}
            }
            if obu.bytes[0] & 2 == 0 {
                append_sized_obu(obu, &mut sample);
            } else {
                sample.extend_from_slice(obu.bytes);
            }
        }
        ensure!(tile_group_seen != Some(false), "missing AV1 tile group");
        let mut configuration_changed = false;
        if let Some(sequence) = sequence {
            let configuration = Configuration::Av1 {
                av1c,
                format: av1::format_from_sequence(sequence, &sample),
                reduced_still_picture_header: sequence.reduced_still_picture_header,
            };
            configuration_changed = self.configuration.as_ref() != Some(&configuration);
            if configuration_changed {
                self.configuration = Some(configuration);
            }
        }
        Ok(Sample {
            data: sample,
            has_picture,
            is_keyframe,
            configuration_changed,
        })
    }
}

fn nal_header(hevc: bool, nal: &[u8]) -> Option<(u8, usize)> {
    let header = *nal.first()?;
    if header & 0x80 != 0 {
        return None;
    }
    if hevc {
        let second = *nal.get(1)?;
        // Farwing sends single-layer base streams; temporal_id_plus1 cannot be zero.
        (nal.len() >= 3 && header & 1 == 0 && second >> 3 == 0 && second & 7 != 0)
            .then_some(((header >> 1) & 63, 2))
    } else {
        let kind = header & 31;
        (nal.len() >= 2 && matches!(kind, 1..=23)).then_some((kind, 1))
    }
}

fn parameter(hevc: bool, kind: u8, nal: &[u8], header_len: usize) -> Option<(u32, Parameter)> {
    if nal.len() > 65536 {
        return None;
    }
    let rbsp = remove_emulation_prevention(nal.get(header_len..)?);
    let (id, parent_id, format) = match (hevc, kind) {
        (false, 7) => {
            let sps = Sps::parse(&rbsp).ok()?;
            (
                sps.seq_parameter_set_id,
                0,
                Some(video_format::parse_h264_sps(nal)?),
            )
        }
        (false, 8) => {
            if rbsp.len() < 2 {
                return None;
            }
            let mut bits = BitReader::new(&rbsp);
            let (id, sps) = (bits.read_ue()?, bits.read_ue()?);
            if id > 255 || sps > 31 {
                return None;
            }
            (id, sps, None)
        }
        (true, 32) => {
            let mut bits = BitReader::new(&rbsp);
            let id = bits.read_bits(4)?;
            bits.skip_bits(8)?; // base layer flags and max_layers_minus1
            let layers = bits.read_bits(3)?;
            bits.read_bit()?; // temporal_id_nesting_flag
            if layers > 6 || bits.read_bits(16)? != 0xffff {
                return None;
            }
            video_format::skip_h265_profile_tier_level(&mut bits, layers as usize)?;
            (id, 0, None)
        }
        (true, 33) => {
            let sps = SeqParameterSet::parse(&rbsp).ok()?;
            (
                sps.sps_id.into(),
                sps.vps_id.into(),
                Some(video_format::parse_h265_sps(nal)?),
            )
        }
        (true, 34) => {
            let pps = PicParameterSet::parse(&rbsp).ok()?;
            (pps.pps_id.into(), pps.sps_id.into(), None)
        }
        _ => return None,
    };
    Some((
        id,
        Parameter {
            nal: nal.to_vec(),
            parent_id,
            format,
        },
    ))
}

fn slice_pps(hevc: bool, kind: u8, ebsp: &[u8]) -> Option<u32> {
    if ebsp.len() < 2 {
        return None;
    }
    // The PPS ID is in the bounded prefix of a slice header. Never unescape
    // the coded picture just to read it.
    let rbsp = remove_emulation_prevention(&ebsp[..ebsp.len().min(64)]);
    let mut bits = BitReader::new(&rbsp);
    if hevc {
        bits.read_bit()?; // first_slice_segment_in_pic_flag
        if matches!(kind, 16..=23) {
            bits.read_bit()?; // no_output_of_prior_pics_flag
        }
    } else {
        bits.read_ue()?; // first_mb_in_slice
        if bits.read_ue()? > 9 {
            return None;
        }
    }
    let pps = bits.read_ue()?;
    (pps <= if hevc { 63 } else { 255 }).then_some(pps)
}

fn av1_config(s: Sequence, sequence: Obu<'_>) -> Vec<u8> {
    let (x, y) = match s.chroma {
        Chroma::Monochrome | Chroma::Yuv420 => (true, true),
        Chroma::Yuv422 => (true, false),
        Chroma::Yuv444 => (false, false),
    };
    let mut av1c = vec![
        0x81, // marker = 1, version = 1
        (s.profile << 5) | s.level_0,
        (u8::from(s.tier_0) << 7)
            | (u8::from(s.depth > 8) << 6)
            | (u8::from(s.depth == 12) << 5)
            | (u8::from(s.chroma == Chroma::Monochrome) << 4)
            | (u8::from(x) << 3)
            | (u8::from(y) << 2)
            | s.chroma_sample_position,
        0, // no initial_presentation_delay in the record
    ];
    append_sized_obu(sequence, &mut av1c);
    av1c
}

fn append_sized_obu(obu: Obu<'_>, out: &mut Vec<u8>) {
    let header = obu.bytes[0];
    out.push(header | 2);
    if header & 4 != 0 {
        out.push(obu.bytes[1]);
    }
    av1::put_leb(obu.payload.len(), out);
    out.extend_from_slice(obu.payload);
}

#[cfg(test)]
mod tests;
