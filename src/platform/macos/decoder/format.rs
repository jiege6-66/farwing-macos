//! CoreMedia descriptions and owned samples for VideoToolbox.

use std::ptr::{self, NonNull};

use objc2_core_foundation::{CFBoolean, CFData, CFDictionary, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMVideoFormatDescription,
    CMVideoFormatDescriptionCreate, CMVideoFormatDescriptionCreateFromH264ParameterSets,
    CMVideoFormatDescriptionCreateFromHEVCParameterSets, kCMBlockBufferAssureMemoryNowFlag,
    kCMFormatDescriptionExtension_FullRangeVideo,
    kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms, kCMVideoCodecType_AV1,
};
use objc2_core_video::*;

use super::bitstream::Configuration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PixelFormat {
    pub(super) fourcc: u32,
    pub(super) bytes_per_sample: usize,
}

impl PixelFormat {
    pub(super) fn for_configuration(config: &Configuration) -> Option<Self> {
        let format = config.format();
        if format.bit_depth_luma != format.bit_depth_chroma {
            return None;
        }
        let fourcc = match (
            format.chroma_format_idc,
            format.bit_depth_luma,
            config.full_range(),
        ) {
            (1, 8, false) => kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            (1, 8, true) => kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
            (1, 10, false) => kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
            (1, 10, true) => kCVPixelFormatType_420YpCbCr10BiPlanarFullRange,
            (3, 8, false) => kCVPixelFormatType_444YpCbCr8BiPlanarVideoRange,
            (3, 8, true) => kCVPixelFormatType_444YpCbCr8BiPlanarFullRange,
            (3, 10, false) => kCVPixelFormatType_444YpCbCr10BiPlanarVideoRange,
            (3, 10, true) => kCVPixelFormatType_444YpCbCr10BiPlanarFullRange,
            _ => return None,
        };
        Some(Self {
            fourcc,
            bytes_per_sample: usize::from(format.bit_depth_luma).div_ceil(8),
        })
    }
}

pub(super) fn description(
    config: &Configuration,
) -> Result<CFRetained<CMVideoFormatDescription>, i32> {
    let mut out = ptr::null();
    let status = match config {
        Configuration::AnnexB { vps, sps, pps, .. } => {
            let nals: &[&[u8]] = if let Some(vps) = vps {
                &[vps, sps, pps]
            } else {
                &[sps, pps]
            };
            let mut pointers = [NonNull::dangling(); 3];
            let mut sizes = [0; 3];
            for (i, nal) in nals.iter().enumerate() {
                pointers[i] = NonNull::from_ref(*nal).cast::<u8>();
                sizes[i] = nal.len();
            }
            let pointers = NonNull::from_mut(&mut pointers).cast();
            let sizes = NonNull::from_mut(&mut sizes).cast();
            // SAFETY: The arrays have nals.len() initialized entries pointing to
            // live parameter NALs, and out is writable. CM copies the parameters.
            unsafe {
                if vps.is_some() {
                    CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                        None,
                        nals.len(),
                        pointers,
                        sizes,
                        4,
                        None,
                        NonNull::from_mut(&mut out),
                    )
                } else {
                    CMVideoFormatDescriptionCreateFromH264ParameterSets(
                        None,
                        nals.len(),
                        pointers,
                        sizes,
                        4,
                        NonNull::from_mut(&mut out),
                    )
                }
            }
        }
        Configuration::Av1 {
            av1c,
            format,
            full_range,
            ..
        } => {
            let av1c = CFData::from_bytes(av1c);
            let atoms = CFDictionary::from_slices(&[&*CFString::from_str("av1C")], &[&*av1c]);
            // SAFETY: These are framework-owned immutable CFString constants.
            let keys = unsafe {
                [
                    kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms,
                    kCMFormatDescriptionExtension_FullRangeVideo,
                ]
            };
            let extensions = CFDictionary::<CFString, CFType>::from_slices(
                &keys,
                &[
                    atoms.as_opaque().as_ref(),
                    CFBoolean::new(*full_range).as_ref(),
                ],
            );
            // SAFETY: The extensions are a dictionary of CFString keys and CFType
            // values; the atom dictionary holds the av1C payload as CFData.
            unsafe {
                CMVideoFormatDescriptionCreate(
                    None,
                    kCMVideoCodecType_AV1,
                    format.visible_width as i32,
                    format.visible_height as i32,
                    Some(extensions.as_opaque()),
                    NonNull::from_mut(&mut out),
                )
            }
        }
    };
    if status != 0 {
        return Err(status);
    }
    // SAFETY: A successful Create returns a +1 format description.
    NonNull::new(out.cast_mut())
        .map(|out| unsafe { CFRetained::from_raw(out) })
        .ok_or(status)
}

pub(super) fn sample_buffer(
    data: &[u8],
    description: &CMFormatDescription,
) -> Result<CFRetained<CMSampleBuffer>, i32> {
    // Use CoreMedia-owned memory: neither a sample nor a reference to its bytes
    // depends on how long a decoder retains the input after the call returns.
    let mut block = ptr::null_mut();
    // SAFETY: Default allocator, no borrowed block, writable output pointer.
    let status = unsafe {
        CMBlockBuffer::create_with_memory_block(
            None,
            ptr::null_mut(),
            data.len(),
            None,
            ptr::null(),
            0,
            data.len(),
            kCMBlockBufferAssureMemoryNowFlag,
            NonNull::from_mut(&mut block),
        )
    };
    if status != 0 {
        return Err(status);
    }
    let block = NonNull::new(block).ok_or(status)?;
    // SAFETY: A successful Create returns a +1 block buffer.
    let block = unsafe { CFRetained::from_raw(block) };
    // SAFETY: Copy exactly the number of bytes in both data and the allocated block.
    let status = unsafe {
        CMBlockBuffer::replace_data_bytes(NonNull::from_ref(data).cast(), &block, 0, data.len())
    };
    if status != 0 {
        return Err(status);
    }
    let mut sample = ptr::null_mut();
    // SAFETY: One ready sample, no timing array/callback, one size entry and
    // valid block/description. The sample retains the block and description.
    let status = unsafe {
        CMSampleBuffer::create(
            None,
            Some(&block),
            true,
            None,
            ptr::null_mut(),
            Some(description),
            1,
            0,
            ptr::null(),
            1,
            &data.len(),
            NonNull::from_mut(&mut sample),
        )
    };
    if status != 0 {
        return Err(status);
    }
    // SAFETY: A successful Create returns a +1 sample buffer.
    NonNull::new(sample)
        .map(|sample| unsafe { CFRetained::from_raw(sample) })
        .ok_or(status)
}
