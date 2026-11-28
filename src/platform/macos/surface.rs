//! Owning decoded surfaces. No GPU device or upload is needed for decode on macOS.

use std::{collections::hash_map::DefaultHasher, hash::Hasher, ops::Range};

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPlaneCount, CVPixelBufferGetWidthOfPlane,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};

use super::decoder::{CpuFormat, CpuFrame, GpuFrame};

/// VideoToolbox chooses its own device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GpuDevice {}

#[derive(Debug)]
pub(crate) struct Surface(Storage);

#[derive(Debug)]
enum Storage {
    PixelBuffer(GpuFrame),
    Cpu {
        data: Bytes,
        width: u32,
        height: u32,
        stride: usize,
        bytes_per_sample: usize,
        format: CpuFormat,
    },
}

#[derive(Clone)]
pub(crate) struct SurfaceWriter;

impl SurfaceWriter {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self)
    }

    pub(crate) fn wrap_decoded_surface(&self, frame: GpuFrame) -> Result<Surface> {
        // The VT callback has already checked the IOSurface and pixel layout.
        Ok(Surface(Storage::PixelBuffer(frame)))
    }

    pub(crate) fn upload_cpu(&self, frame: &CpuFrame) -> Result<Surface> {
        let CpuFrame {
            width,
            height,
            coded_width,
            coded_height,
            format,
            ..
        } = *frame;
        ensure!(
            width > 0 && height > 0 && width <= coded_width && height <= coded_height,
            "visible area outside the CPU frame"
        );
        let bps = match format {
            CpuFormat::Nv12 => 1,
            CpuFormat::P010 => 2,
            _ => 4,
        };
        let stride = coded_width as usize * bps;
        let rows = match format {
            CpuFormat::Nv12 | CpuFormat::P010 => {
                ensure!(
                    coded_width % 2 == 0 && coded_height % 2 == 0,
                    "unaligned 4:2:0 CPU frame"
                );
                coded_height as usize + coded_height as usize / 2
            }
            _ => coded_height as usize,
        };
        ensure!(
            stride.checked_mul(rows) == Some(frame.data.len()),
            "invalid CPU frame length"
        );
        Ok(Surface(Storage::Cpu {
            data: frame.data.clone(),
            width,
            height,
            stride,
            bytes_per_sample: bps,
            format,
        }))
    }
}

impl Surface {
    pub(crate) fn luma_hash(&self, w: u32, h: u32) -> Result<u64> {
        // Hash luma once per visible row, with the same sample layout for Mac
        // hardware and software. 4:2:0 matches Windows byte-for-byte, but
        // 4:4:4 is not comparable: Windows hashes the packed pixel bytes.
        let mut hasher = DefaultHasher::new();
        match &self.0 {
            Storage::PixelBuffer(frame) => {
                ensure!(
                    w <= frame.width() && h <= frame.height(),
                    "hash area outside the visible frame"
                );
                let lock = ReadLock::new(&frame.pixel_buffer)?;
                let (x, y) = frame.visible_origin;
                let bps = frame.bytes_per_sample;
                let columns = x as usize..(x + w) as usize;
                for row in y..y + h {
                    hasher.write(lock.row(0, row as usize, columns.clone(), bps)?);
                }
            }
            Storage::Cpu {
                data,
                width,
                height,
                stride,
                bytes_per_sample,
                format,
            } => {
                ensure!(
                    w <= *width && h <= *height,
                    "hash area outside the visible frame"
                );
                let mut luma = Vec::new();
                let bytes = w as usize * bytes_per_sample;
                for row in 0..h as usize {
                    let start = row * stride;
                    let pixels = &data[start..start + bytes];
                    let luma_row = match format {
                        CpuFormat::Nv12 | CpuFormat::P010 => pixels,
                        CpuFormat::Ayuv => {
                            // farwing_codec stores V, U, Y, A bytes.
                            luma.clear();
                            luma.extend(pixels.chunks_exact(4).map(|p| p[2]));
                            &luma
                        }
                        CpuFormat::Y410 => {
                            // U:Y:V:A from least to most significant bits. x444
                            // (like x420/P010) stores Y in the upper 10 bits of LE u16.
                            luma.clear();
                            luma.extend(pixels.chunks_exact(4).flat_map(|p| {
                                let packed = u32::from_le_bytes(p.try_into().unwrap());
                                let y = ((packed >> 10) & 0x3ff) as u16;
                                (y << 6).to_le_bytes()
                            }));
                            &luma
                        }
                    };
                    hasher.write(luma_row);
                }
            }
        }
        Ok(hasher.finish())
    }
}

struct ReadLock<'a>(&'a CVPixelBuffer);
impl<'a> ReadLock<'a> {
    fn new(buffer: &'a CVPixelBuffer) -> Result<Self> {
        // SAFETY: The retained buffer is live; ReadOnly permits no writes.
        let status =
            unsafe { CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly) };
        ensure!(status == 0, "CVPixelBuffer read lock failed: {status}");
        Ok(Self(buffer))
    }

    fn row(
        &self,
        plane: usize,
        y: usize,
        x: Range<usize>,
        bytes_per_sample: usize,
    ) -> Result<&[u8]> {
        let buffer = self.0;
        ensure!(
            plane < CVPixelBufferGetPlaneCount(buffer),
            "missing pixel buffer plane"
        );
        ensure!(
            y < CVPixelBufferGetHeightOfPlane(buffer, plane)
                && x.end <= CVPixelBufferGetWidthOfPlane(buffer, plane),
            "read outside pixel buffer plane"
        );
        let stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, plane);
        ensure!(
            x.end * bytes_per_sample <= stride,
            "invalid pixel buffer stride"
        );
        let base =
            std::ptr::NonNull::new(CVPixelBufferGetBaseAddressOfPlane(buffer, plane).cast::<u8>())
                .context("no pixel buffer plane base address")?;
        // SAFETY: The plane is locked and the requested span is within its initialized
        // pixels (excludes stride padding); the slice cannot outlive this lock.
        Ok(unsafe {
            std::slice::from_raw_parts(
                base.as_ptr().add(y * stride + x.start * bytes_per_sample),
                x.len() * bytes_per_sample,
            )
        })
    }
}
impl Drop for ReadLock<'_> {
    fn drop(&mut self) {
        // SAFETY: Exactly one unlock for the successful read-only lock in new().
        unsafe { CVPixelBufferUnlockBaseAddress(self.0, CVPixelBufferLockFlags::ReadOnly) };
    }
}

#[cfg(test)]
pub(super) fn test_pixels<const N: usize>(
    frame: &GpuFrame,
    points: [(u32, u32); N],
) -> Result<[[u16; 3]; N]> {
    let lock = ReadLock::new(&frame.pixel_buffer)?;
    let (left, top) = frame.visible_origin;
    let bps = frame.bytes_per_sample;
    let sub = match frame.pixel_format().to_be_bytes() {
        [b'4', b'2', b'0', _] | [b'x', b'4' | b'f', b'2', b'0'] => 2,
        _ => 1,
    };
    let mut pixels = [[0; 3]; N];
    for ((x, y), pixel) in points.into_iter().zip(&mut pixels) {
        let (x, y) = ((x + left) as usize, (y + top) as usize);
        let yuv_y = lock.row(0, y, x..x + 1, bps)?;
        let uvx = x / sub;
        let uv = lock.row(1, y / sub, uvx..uvx + 1, bps * 2)?;
        let sample = |bytes: &[u8]| {
            if bps == 1 {
                u16::from(bytes[0])
            } else {
                u16::from_le_bytes([bytes[0], bytes[1]]) >> 8
            }
        };
        *pixel = [sample(yuv_y), sample(uv), sample(&uv[bps..])];
    }
    Ok(pixels)
}

#[cfg(test)]
pub(super) fn test_luma_words<const N: usize>(
    frame: &GpuFrame,
    points: [(u32, u32); N],
) -> Result<[u16; N]> {
    assert_eq!(frame.bytes_per_sample, 2);
    let lock = ReadLock::new(&frame.pixel_buffer)?;
    let (left, top) = frame.visible_origin;
    let mut words = [0; N];
    for ((x, y), word) in points.into_iter().zip(&mut words) {
        let x = (left + x) as usize;
        let bytes = lock.row(0, (top + y) as usize, x..x + 1, 2)?;
        *word = u16::from_le_bytes(bytes.try_into().unwrap());
    }
    Ok(words)
}

#[test]
fn cpu_hash_respects_visible_rows_and_ignores_nv12_chroma() {
    fn send<T: Send>() {}
    send::<Surface>();
    send::<SurfaceWriter>();
    let writer = SurfaceWriter::new().unwrap();
    let mut frame = CpuFrame {
        pts: 0,
        width: 2,
        height: 2,
        coded_width: 4,
        coded_height: 2,
        format: CpuFormat::Nv12,
        data: Bytes::from_static(&[1, 2, 0, 0, 3, 4, 0, 0, 7, 8, 9, 10]),
    };
    let mut expected = DefaultHasher::new();
    expected.write(&[1, 2]);
    expected.write(&[3, 4]);
    let surface = writer.upload_cpu(&frame).unwrap();
    assert_eq!(surface.luma_hash(2, 2).unwrap(), expected.finish());
    assert!(surface.luma_hash(3, 2).is_err());
    frame.data = Bytes::new();
    assert!(writer.upload_cpu(&frame).is_err());
}

#[test]
fn y410_hash_uses_only_msb_aligned_luma_in_visible_rows() {
    let pixel = |u: u32, y: u32, v: u32, a: u32| (u | y << 10 | v << 20 | a << 30).to_le_bytes();
    let data = [
        pixel(1023, 1, 300, 3),
        pixel(0, 0x155, 1, 1),
        pixel(0, 9, 0, 0),
        pixel(1, 0x2aa, 1023, 0),
        pixel(70, 0x3ff, 90, 2),
        pixel(0, 7, 0, 0),
    ]
    .concat();
    let frame = CpuFrame {
        pts: 0,
        width: 2,
        height: 2,
        coded_width: 3,
        coded_height: 2,
        format: CpuFormat::Y410,
        data: data.into(),
    };
    let mut expected = DefaultHasher::new();
    expected.write(&[0x40, 0x00, 0x40, 0x55]);
    expected.write(&[0x80, 0xaa, 0xc0, 0xff]);
    let surface = SurfaceWriter::new().unwrap().upload_cpu(&frame).unwrap();
    assert_eq!(surface.luma_hash(2, 2).unwrap(), expected.finish());
}
