// SPDX-License-Identifier: MIT
// Derived from oxideav-h265 0.0.10, Copyright (c) 2026 Karpelès Lab Inc.
// Retained syntax/metadata primitives used by this client; see COPYING.OxideAV in this crate.

use crate::bitreader::{BitReader, BitReaderError};

use crate::hrd::{HrdError, HrdParameters};

pub const HEVC_MAX_SUB_LAYERS: usize = 7;

pub const HEVC_VPS_MAX_NUM_LAYER_SETS: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VpsError {
    Truncated,
    ReservedFieldMismatch { got: u16 },
    ValueOutOfRange { field: &'static str, got: u32 },
    Bitstream(BitReaderError),
    Hrd(HrdError),
}

impl core::fmt::Display for VpsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => f.write_str("VPS RBSP truncated"),
            Self::ReservedFieldMismatch { got } => write!(
                f,
                "vps_reserved_0xffff_16bits was 0x{got:04X}, expected 0xFFFF"
            ),
            Self::ValueOutOfRange { field, got } => {
                write!(f, "syntax element {field} out of range: {got}")
            }
            Self::Bitstream(e) => write!(f, "bitstream error during VPS parse: {e}"),
            Self::Hrd(e) => write!(f, "hrd_parameters() error inside VPS: {e}"),
        }
    }
}

impl std::error::Error for VpsError {}

impl From<BitReaderError> for VpsError {
    fn from(e: BitReaderError) -> Self {
        match e {
            BitReaderError::EndOfBuffer => Self::Truncated,
            other => Self::Bitstream(other),
        }
    }
}

impl From<HrdError> for VpsError {
    fn from(e: HrdError) -> Self {
        // Surface a bitstream-truncation reported through the HRD path
        // as the VPS-level Truncated variant so callers can keep their
        // single-pattern truncation handler. Any other HRD failure mode
        // is opaque to the VPS — preserve it verbatim.
        match e {
            HrdError::Truncated => Self::Truncated,
            other => Self::Hrd(other),
        }
    }
}

/// Validate a single-layer VPS RBSP (H.265 §7.3.2.1) and return its ID.
/// Only the base syntax is interpreted; extension_data_flag bits are opaque.
pub fn parse_base_layer_vps_id(rbsp: &[u8]) -> Result<u8, VpsError> {
    let mut br = BitReader::new(rbsp);
    let id = br.u(4)? as u8;
    br.skip(2)?; // base_layer_internal_flag and base_layer_available_flag
    let max_layers_minus1 = br.u(6)?;
    let max_sub_layers_minus1 = br.u(3)? as u8;
    let temporal_nesting = br.u1()?;
    bounded("vps_max_layers_minus1", max_layers_minus1, 0)?;
    bounded("vps_max_sub_layers_minus1", max_sub_layers_minus1.into(), 6)?;
    if max_sub_layers_minus1 == 0 && temporal_nesting == 0 {
        return Err(VpsError::ValueOutOfRange {
            field: "vps_temporal_id_nesting_flag",
            got: 0,
        });
    }
    let reserved = br.u(16)? as u16;
    if reserved != 0xffff {
        return Err(VpsError::ReservedFieldMismatch { got: reserved });
    }
    ProfileTierLevel::parse(&mut br, true, max_sub_layers_minus1)?;

    let first = if br.u1()? == 0 {
        max_sub_layers_minus1
    } else {
        0
    };
    for _ in first..=max_sub_layers_minus1 {
        let buffering = br.ue()?;
        bounded("vps_max_num_reorder_pics", br.ue()?, buffering)?;
        br.ue()?; // vps_max_latency_increase_plus1
    }
    let max_layer_id = br.u(6)?;
    bounded("vps_max_layer_id", max_layer_id, 62)?;
    let layer_sets = br.ue()?;
    bounded(
        "vps_num_layer_sets_minus1",
        layer_sets,
        HEVC_VPS_MAX_NUM_LAYER_SETS as u32 - 1,
    )?;
    br.skip((layer_sets * (max_layer_id + 1)) as usize)?; // layer_id_included_flag
    if br.u1()? != 0 {
        for field in ["vps_num_units_in_tick", "vps_time_scale"] {
            if br.u(32)? == 0 {
                return Err(VpsError::ValueOutOfRange { field, got: 0 });
            }
        }
        if br.u1()? != 0 {
            br.ue()?; // vps_num_ticks_poc_diff_one_minus1
        }
        let hrd_count = br.ue()?;
        bounded("vps_num_hrd_parameters", hrd_count, layer_sets + 1)?;
        let mut common = None;
        for index in 0..hrd_count {
            bounded("hrd_layer_set_idx", br.ue()?, layer_sets)?;
            let common_present = index == 0 || br.u1()? != 0;
            let hrd = HrdParameters::parse(
                &mut br,
                common_present,
                max_sub_layers_minus1,
                common.as_ref(),
            )?;
            common = hrd.common.or(common);
        }
    }
    let extension = br.u1()? != 0;
    if !br.has_rbsp_end(extension) {
        return Err(VpsError::ValueOutOfRange {
            field: "rbsp_trailing_bits",
            got: 0,
        });
    }
    Ok(id)
}

fn bounded(field: &'static str, got: u32, max: u32) -> Result<(), VpsError> {
    if got > max {
        return Err(VpsError::ValueOutOfRange { field, got });
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileTierLevel {
    pub general_profile_space: u8,
    pub general_tier_flag: bool,
    pub general_profile_idc: u8,
    pub general_level_idc: u8,
    pub sub_layer_profile_present: [bool; HEVC_MAX_SUB_LAYERS],
    pub sub_layer_level_present: [bool; HEVC_MAX_SUB_LAYERS],
    pub sub_layer_level_idc: [u8; HEVC_MAX_SUB_LAYERS],
}

impl ProfileTierLevel {
    pub fn parse(
        br: &mut BitReader<'_>,
        profile_present_flag: bool,
        max_num_sub_layers_minus1: u8,
    ) -> Result<Self, VpsError> {
        let mut ptl = Self {
            general_profile_space: 0,
            general_tier_flag: false,
            general_profile_idc: 0,
            general_level_idc: 0,
            sub_layer_profile_present: [false; HEVC_MAX_SUB_LAYERS],
            sub_layer_level_present: [false; HEVC_MAX_SUB_LAYERS],
            sub_layer_level_idc: [0; HEVC_MAX_SUB_LAYERS],
        };

        if profile_present_flag {
            ptl.general_profile_space = br.u(2)? as u8;
            ptl.general_tier_flag = br.u1()? != 0;
            ptl.general_profile_idc = br.u(5)? as u8;
            // 32 compatibility flags — skipped wholesale; the calling
            // application can re-parse them from the bit position if
            // needed later.
            br.skip(32)?;
            // progressive / interlaced / non_packed / frame_only
            br.skip(4)?;
            // The conditional block beneath these flags always consumes
            // exactly 43 bits regardless of profile_idc (per the
            // `/* not affected by this condition */` comment in §7.3.3
            // — the chroma-constraint, range-extension, and reserved
            // alternatives all sum to 43 bits).
            br.skip(43)?;
            // general_inbld_flag OR general_reserved_zero_bit — always 1 bit.
            br.skip(1)?;
        }
        // general_level_idc is always present (no `profilePresentFlag`
        // guard in the §7.3.3 syntax).
        ptl.general_level_idc = br.u(8)? as u8;

        // Per-sub-layer present-flag gates: 2 bits per sublayer up to
        // (but excluding) maxNumSubLayersMinus1.
        let max = max_num_sub_layers_minus1 as usize;
        for i in 0..max {
            let prof = br.u1()? != 0;
            let lvl = br.u1()? != 0;
            ptl.sub_layer_profile_present[i] = prof;
            ptl.sub_layer_level_present[i] = lvl;
        }

        // §7.3.3: if maxNumSubLayersMinus1 > 0, then for i in
        // max..8: reserved_zero_2bits — exactly 2 bits each — to keep
        // the per-sub-layer body byte-aligned regardless of how many
        // sublayers were actually signalled.
        if max_num_sub_layers_minus1 > 0 {
            for _ in max..8 {
                br.skip(2)?;
            }
        }

        // Per-sub-layer profile/level body for each i in 0..max.
        for i in 0..max {
            if ptl.sub_layer_profile_present[i] {
                // 2 + 1 + 5 + 32 + 4 + 43 + 1 = 88 bits, identical
                // layout to the general profile block above.
                br.skip(88)?;
            }
            if ptl.sub_layer_level_present[i] {
                ptl.sub_layer_level_idc[i] = br.u(8)? as u8;
            }
        }

        Ok(ptl)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SubLayerOrderingInfo {
    pub max_dec_pic_buffering_minus1: u32,
    pub max_num_reorder_pics: u32,
    pub max_latency_increase_plus1: u32,
}
