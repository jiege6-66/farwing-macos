//! Prepares one Farwing access unit at a time for CoreMedia. No FFI or session state.
//!
//! HEVC random access is not always closed: `Sample::hevc_vcl_nal_type` is 19/20
//! for IDR, 16–18 for BLA and 21 for CRA. T2b must handle associated RASL
//! (types 8/9) on entry at CRA/BLA; this module neither sends nor drops those
//! pictures on behalf of the decoder. AVC recovery-point SEI is not an IDR.

use std::{borrow::Cow, collections::BTreeMap};

use anyhow::{Context, Result, anyhow, bail, ensure};
use bytes::Bytes;
use farwing_codec::syntax::{
    av1::{
        Chroma, Sequence,
        obu::{Obu, Units},
        syntax::{FrameStart, FrameType},
    },
    h264::{nal::AnnexBSplitter, pps::Pps, sps::Sps},
    hevc::{pps::PicParameterSet, sps::SeqParameterSet, vps::parse_base_layer_vps_id},
};

use crate::media::{
    VideoCodec, av1,
    decode_api::DecodeError,
    video_format::{self, BitReader, VideoFormatSignature, remove_emulation_prevention},
};

#[derive(Debug, thiserror::Error)]
pub(crate) enum BitstreamError {
    #[error("missing codec parameters: {0}")]
    MissingParameters(&'static str),
    #[error("invalid decode input: {0}")]
    InvalidInput(#[source] anyhow::Error),
    #[error("unsupported bitstream: {0}")]
    Unsupported(&'static str),
}

impl BitstreamError {
    pub(crate) fn decode_error(&self) -> DecodeError {
        match self {
            Self::MissingParameters(_) => DecodeError::NeedKeyframe,
            Self::InvalidInput(_) => DecodeError::InvalidInput,
            Self::Unsupported(_) => DecodeError::Unsupported,
        }
    }
}

impl From<BitstreamError> for DecodeError {
    fn from(error: BitstreamError) -> Self {
        error.decode_error()
    }
}

impl From<anyhow::Error> for BitstreamError {
    fn from(error: anyhow::Error) -> Self {
        error.downcast::<Self>().unwrap_or_else(Self::InvalidInput)
    }
}

type BitstreamResult<T> = std::result::Result<T, BitstreamError>;

/// Compare this with the configuration installed in the decoder, not just
/// with the previous packet's configuration (parameter-only packets can update it).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Configuration {
    AnnexB {
        /// Raw NALs, including NAL headers, but without start codes or lengths.
        vps: Option<Bytes>,
        sps: Bytes,
        pps: Bytes,
        format: VideoFormatSignature,
        full_range: bool,
    },
    Av1 {
        /// AV1CodecConfigurationRecord payload, without the atom size/type.
        av1c: Vec<u8>,
        format: VideoFormatSignature,
        full_range: bool,
        reduced_still_picture_header: bool,
    },
}

impl Configuration {
    pub(crate) fn format(&self) -> VideoFormatSignature {
        match self {
            Self::AnnexB { format, .. } | Self::Av1 { format, .. } => *format,
        }
    }

    pub(super) fn full_range(&self) -> bool {
        match self {
            Self::AnnexB { full_range, .. } | Self::Av1 { full_range, .. } => *full_range,
        }
    }
}

pub(crate) struct Sample {
    /// Four-byte big-endian NAL lengths, or sized AV1 OBUs.
    pub(crate) data: Vec<u8>,
    pub(crate) has_picture: bool,
    /// AVC IDR, HEVC IRAP, or AV1 with an in-band sequence header followed
    /// by a shown key frame as the first frame (AV1-ISOBMFF §2.4).
    pub(crate) is_keyframe: bool,
    pub(crate) configuration_changed: bool,
    /// First VCL NAL type for HEVC pictures; `None` for other codecs or no picture.
    pub(crate) hevc_vcl_nal_type: Option<u8>,
}

#[derive(Clone)]
struct Parameter {
    nal: Bytes,
    parent_id: u32,
    format: Option<VideoFormatSignature>,
    full_range: bool,
    avc_pps_chroma444: Option<bool>,
}

#[derive(Clone, Default)]
struct Parameters {
    verified: BTreeMap<(u8, u32), Parameter>,
    // An AVC PPS can arrive before its SPS. It must not shadow a verified PPS.
    pending_avc_pps: BTreeMap<u32, Parameter>,
}

impl Parameters {
    fn insert(&mut self, kind: u8, id: u32, mut entry: Parameter, hevc: bool) -> Result<()> {
        if !hevc && kind == 8 {
            let rbsp = remove_emulation_prevention(&entry.nal[1..]);
            let parent = self
                .verified
                .get(&(7, entry.parent_id))
                .and_then(|sps| sps.format);
            if let Some(format) = parent {
                Pps::parse_complete(&rbsp, format.chroma_format_idc.into())
                    .context("invalid H.264 PPS")?;
                entry.avc_pps_chroma444 = Some(format.chroma_format_idc == 3);
                self.pending_avc_pps.remove(&id);
            } else {
                // Reject truncation now even if the precise scaling-list syntax
                // (4:4:4 versus all others) still depends on the parent.
                Pps::parse_complete(&rbsp, 1)
                    .or_else(|_| Pps::parse_complete(&rbsp, 3))
                    .context("invalid H.264 PPS")?;
                self.pending_avc_pps.insert(id, entry);
                return Ok(());
            }
        } else if !hevc && kind == 7 {
            let chroma = entry
                .format
                .context("missing H.264 SPS format")?
                .chroma_format_idc;
            for pps in self
                .pending_avc_pps
                .values()
                .filter(|pps| pps.parent_id == id)
            {
                validate_avc_pps(pps, chroma)?;
            }
            self.pending_avc_pps.retain(|&pps_id, pps| {
                if pps.parent_id == id {
                    pps.avc_pps_chroma444 = Some(chroma == 3);
                    self.verified.insert((8, pps_id), pps.clone());
                    false
                } else {
                    true
                }
            });
        }
        self.verified.insert((kind, id), entry);
        Ok(())
    }

    fn configuration(&self, hevc: bool, pps_id: u32) -> Option<Configuration> {
        let pps = self.verified.get(&(if hevc { 34 } else { 8 }, pps_id))?;
        let sps = self
            .verified
            .get(&(if hevc { 33 } else { 7 }, pps.parent_id))?;
        let format = sps.format?;
        let chroma = format.chroma_format_idc;
        if pps
            .avc_pps_chroma444
            .is_some_and(|is_444| is_444 != (chroma == 3))
        {
            // The cached PPS was verified under a different SPS. Only reuse
            // it if its scaling-list syntax also fits the SPS at selection.
            validate_avc_pps(pps, chroma).ok()?;
        }
        let vps = if hevc {
            Some(self.verified.get(&(32, sps.parent_id))?.nal.clone())
        } else {
            None
        };
        Some(Configuration::AnnexB {
            vps,
            sps: sps.nal.clone(),
            pps: pps.nal.clone(),
            format,
            full_range: sps.full_range,
        })
    }

    fn first_configuration(&self, hevc: bool) -> Option<(u32, Configuration)> {
        let pps_kind = if hevc { 34 } else { 8 };
        self.verified
            .range((pps_kind, 0)..=(pps_kind, u32::MAX))
            .find_map(|(&(_, id), _)| Some((id, self.configuration(hevc, id)?)))
    }
}

fn validate_avc_pps(pps: &Parameter, chroma: u8) -> Result<()> {
    Pps::parse_complete(&remove_emulation_prevention(&pps.nal[1..]), chroma.into())
        .context("invalid H.264 PPS for its SPS")?;
    Ok(())
}

pub(crate) struct Bitstream {
    codec: VideoCodec,
    parameters: Parameters,
    active_pps: Option<u32>,
    configuration: Option<Configuration>,
}

impl Bitstream {
    /// Farwing extradata is empty, Annex-B parameter sets, or AV1 sequence OBUs,
    /// not a container's avcC/hvcC/av1C record.
    pub(crate) fn new(codec: VideoCodec, extra_data: &[u8]) -> BitstreamResult<Self> {
        let mut stream = Self {
            codec,
            parameters: Parameters::default(),
            active_pps: None,
            configuration: None,
        };
        if !extra_data.is_empty() && stream.prepare(extra_data)?.has_picture {
            return Err(BitstreamError::InvalidInput(anyhow!(
                "picture in codec extradata"
            )));
        }
        Ok(stream)
    }

    pub(crate) fn configuration(&self) -> Option<&Configuration> {
        self.configuration.as_ref()
    }

    /// Call with one already framed access unit (one temporal unit for AV1).
    /// An error leaves both the parameter cache and configuration untouched.
    /// Validation of coded picture data, beyond framing and headers, belongs to VT.
    pub(crate) fn prepare(&mut self, data: &[u8]) -> BitstreamResult<Sample> {
        if data.is_empty() {
            return Err(BitstreamError::InvalidInput(anyhow!("empty access unit")));
        }
        let sample = if self.codec == VideoCodec::Av1 {
            self.prepare_av1(data)
        } else {
            self.prepare_annex_b(data)
        }?;
        Ok(sample)
    }

    fn prepare_annex_b(&mut self, data: &[u8]) -> Result<Sample> {
        let hevc = self.codec == VideoCodec::H265;
        let leading_zeroes = data.iter().take_while(|&&byte| byte == 0).count();
        ensure!(
            leading_zeroes >= 2 && data.get(leading_zeroes) == Some(&1),
            "missing Annex-B start code"
        );
        // Normal VCL-only packets borrow the cache. Parameter updates use a
        // shallow snapshot and commit only after the entire AU is accepted.
        let mut parameters = Cow::Borrowed(&self.parameters);
        let mut sample = Vec::with_capacity(data.len());
        let mut picture = None;
        for nal in AnnexBSplitter::new(data) {
            let (kind, header_len) = nal_header(hevc, nal).context("invalid or truncated NAL")?;
            if !hevc && kind == 13 {
                bail!(BitstreamError::Unsupported(
                    "AVC SPS extension (NAL type 13)"
                ));
            }
            ensure!(
                !hevc || !matches!(kind, 22 | 23),
                "reserved HEVC VCL NAL type {kind}"
            );
            if if hevc {
                matches!(kind, 32..=34)
            } else {
                matches!(kind, 7 | 8)
            } {
                let (id, entry) = parameter(hevc, kind, nal, header_len)?;
                parameters.to_mut().insert(kind, id, entry, hevc)?;
                if let Some((pps_id, _, ref at_vcl)) = picture {
                    // Unrelated IDs and identical reannouncements can be
                    // cached for future AUs, but must not change this chain.
                    ensure!(
                        parameters.configuration(hevc, pps_id).as_ref() == Some(at_vcl),
                        "parameter set changes the active chain after VCL"
                    );
                }
                continue;
            }
            if if hevc {
                kind <= 31
            } else {
                matches!(kind, 1 | 2 | 5)
            } {
                let id = slice_pps(hevc, kind, &nal[header_len..])
                    .context("invalid or truncated slice header")?;
                if let Some((pps_id, _, _)) = picture {
                    ensure!(pps_id == id, "multiple PPS IDs in one picture");
                } else {
                    let config = parameters.configuration(hevc, id).ok_or(
                        BitstreamError::MissingParameters(
                            "Annex-B picture references an incomplete chain",
                        ),
                    )?;
                    picture = Some((id, kind, config));
                }
            }
            // Keep AUD, SEI, EOS and other non-parameter-set NALs in order.
            sample.extend_from_slice(&u32::try_from(nal.len())?.to_be_bytes());
            sample.extend_from_slice(nal);
        }

        let (pps_id, kind, configuration) = if let Some((id, kind, config)) = picture {
            (Some(id), Some(kind), Some(config))
        } else if let Some(id) = self.active_pps {
            (Some(id), None, parameters.configuration(hevc, id))
        } else if let Some((id, config)) = parameters.first_configuration(hevc) {
            (Some(id), None, Some(config))
        } else {
            (None, None, None)
        };
        if let Cow::Owned(parameters) = parameters {
            self.parameters = parameters;
        }
        self.active_pps = pps_id;
        let configuration_changed = self.configuration != configuration;
        if configuration_changed {
            self.configuration = configuration;
        }
        Ok(Sample {
            data: sample,
            has_picture: kind.is_some(),
            is_keyframe: if hevc {
                matches!(kind, Some(16..=21))
            } else {
                kind == Some(5)
            },
            configuration_changed,
            hevc_vcl_nal_type: if hevc { kind } else { None },
        })
    }

    fn prepare_av1(&mut self, data: &[u8]) -> Result<Sample> {
        let units = Units::with_unsized_last(data)?;
        let mut sample = Vec::with_capacity(data.len());
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
        let mut split_frame = None;
        for (i, obu) in units.enumerate() {
            let obu = obu?;
            let layer = (obu.temporal_id, obu.spatial_id);
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
                    ensure!(
                        i == 0 && obu.payload.is_empty(),
                        "unexpected AV1 temporal delimiter"
                    );
                    continue;
                }
                3 | 6 => {
                    ensure!(
                        !matches!(split_frame, Some((_, false))),
                        "missing AV1 tile group"
                    );
                    let reduced = reduced.ok_or(BitstreamError::MissingParameters(
                        "AV1 picture has no sequence header",
                    ))?;
                    let start = FrameStart::parse(obu, reduced).map_err(anyhow::Error::msg)?;
                    split_frame = (obu.kind == 3 && matches!(start, FrameStart::New { .. }))
                        .then_some((layer, false));
                    if !has_picture {
                        is_keyframe = sequence.is_some()
                            && matches!(
                                start,
                                FrameStart::New {
                                    frame_type: FrameType::KeyFrame,
                                    show_frame: true,
                                }
                            );
                        has_picture = true;
                    }
                }
                4 => {
                    ensure!(
                        !obu.payload.is_empty() && split_frame.is_some_and(|(id, _)| id == layer),
                        "unexpected, empty or mismatched-layer AV1 tile group"
                    );
                    split_frame = Some((layer, true));
                }
                _ => {}
            }
            if obu.bytes[0] & 2 == 0 {
                append_sized_obu(obu, &mut sample);
            } else {
                sample.extend_from_slice(obu.bytes);
            }
        }
        ensure!(
            !matches!(split_frame, Some((_, false))),
            "missing AV1 tile group"
        );
        let mut configuration_changed = false;
        if let Some(sequence) = sequence {
            let configuration = Configuration::Av1 {
                av1c,
                format: av1::format_from_sequence(sequence, &sample),
                full_range: sequence.full_range,
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
            hevc_vcl_nal_type: None,
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
        let kind = (header >> 1) & 63;
        // Farwing sends single-layer base streams; temporal_id_plus1 cannot be zero.
        (header & 1 == 0
            && second >> 3 == 0
            && second & 7 != 0
            && (nal.len() >= 3 || matches!(kind, 36 | 37)))
        .then_some((kind, 2))
    } else {
        let kind = header & 31;
        (matches!(kind, 1..=23) && (nal.len() >= 2 || matches!(kind, 10 | 11))).then_some((kind, 1))
    }
}

fn parameter(hevc: bool, kind: u8, nal: &[u8], header_len: usize) -> Result<(u32, Parameter)> {
    ensure!(nal.len() <= 65536, "parameter NAL too large");
    let rbsp = remove_emulation_prevention(&nal[header_len..]);
    // AVC and HEVC infer video_full_range_flag = 0 when VUI doesn't signal it.
    let mut full_range = false;
    let (id, parent_id, format) = match (hevc, kind) {
        (false, 7) => {
            let sps = Sps::parse_complete(&rbsp).context("invalid H.264 SPS")?;
            full_range = sps
                .vui
                .and_then(|vui| vui.video_signal_type)
                .is_some_and(|signal| signal.video_full_range_flag);
            let format = video_format::parse_h264_sps(nal).context("invalid H.264 SPS format")?;
            (sps.seq_parameter_set_id, 0, Some(format))
        }
        (false, 8) => {
            let mut bits = BitReader::new(&rbsp);
            let id = bits.read_ue().context("missing H.264 PPS id")?;
            let sps = bits
                .read_ue()
                .context("missing parent SPS id in H.264 PPS")?;
            ensure!(id <= 255 && sps <= 31, "invalid H.264 PPS or parent id");
            (id, sps, None)
        }
        (true, 32) => (
            parse_base_layer_vps_id(&rbsp)
                .context("invalid HEVC VPS")?
                .into(),
            0,
            None,
        ),
        (true, 33) => {
            let sps = SeqParameterSet::parse_complete(&rbsp).context("invalid HEVC SPS")?;
            ensure!(
                sps.extension_flags
                    .is_none_or(|flags| !(flags.sps_multilayer_extension_flag
                        || flags.sps_3d_extension_flag)),
                BitstreamError::Unsupported("HEVC multilayer or 3D SPS extension")
            );
            full_range = sps
                .vui_parameters
                .and_then(|vui| vui.video_signal_type)
                .is_some_and(|signal| signal.video_full_range_flag);
            let format = video_format::parse_h265_sps(nal).context("invalid HEVC SPS format")?;
            (sps.sps_id.into(), sps.vps_id.into(), Some(format))
        }
        (true, 34) => {
            let pps = PicParameterSet::parse_complete(&rbsp).context("invalid HEVC PPS")?;
            ensure!(
                pps.extension_flags
                    .is_none_or(|flags| !(flags.pps_multilayer_extension_flag
                        || flags.pps_3d_extension_flag)),
                BitstreamError::Unsupported("HEVC multilayer or 3D PPS extension")
            );
            (pps.pps_id.into(), pps.sps_id.into(), None)
        }
        _ => bail!("not a parameter NAL"),
    };
    Ok((
        id,
        Parameter {
            nal: Bytes::copy_from_slice(nal),
            parent_id,
            format,
            full_range,
            avc_pps_chroma444: None,
        },
    ))
}

fn slice_pps(hevc: bool, kind: u8, ebsp: &[u8]) -> Option<u32> {
    if ebsp.len() < 2 {
        return None;
    }
    // The PPS ID is in the bounded prefix of a slice header. Never unescape
    // the coded picture just to read it.
    let mut rbsp = [0u8; 64];
    let mut len = 0;
    let mut zeroes = 0;
    for &byte in ebsp.iter().take(rbsp.len()) {
        if zeroes >= 2 && byte == 3 {
            zeroes = 0;
            continue;
        }
        rbsp[len] = byte;
        len += 1;
        zeroes = if byte == 0 { zeroes + 1 } else { 0 };
    }
    let mut bits = BitReader::new(&rbsp[..len]);
    if hevc {
        bits.read_bit()?; // first_slice_segment_in_pic_flag
        if matches!(kind, 16..=21) {
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
