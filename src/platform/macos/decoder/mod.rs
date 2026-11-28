//! macOS VideoToolbox and low-delay software H.264. No implicit fallback.

use std::{collections::VecDeque, fmt};

use bytes::Bytes;
use objc2_core_foundation::CFRetained;
use objc2_core_video::CVPixelBuffer;

use crate::media::{
    VideoCodec,
    decode_api::{DecodeError, DecoderMode, DecoderNotification, VideoDecoderConfig},
    video_format::VideoFormatSignature,
};

mod bitstream;
mod format;
#[cfg(test)]
mod tests;
mod vt;

pub(crate) struct GpuFrame {
    pts: i64,
    width: u32,
    height: u32,
    pub(super) visible_origin: (u32, u32),
    pub(super) bytes_per_sample: usize,
    pub(super) pixel_buffer: CFRetained<CVPixelBuffer>,
}

// SAFETY: VT hands off a retained, read-only image. CoreVideo manages the
// buffer lifetime across threads, and CPU reads require a CoreVideo read lock.
// This is an ownership transfer, not Sync access to a mutable CVBuffer.
unsafe impl Send for GpuFrame {}

impl GpuFrame {
    pub(crate) fn pts(&self) -> i64 {
        self.pts
    }
    pub(crate) fn width(&self) -> u32 {
        self.width
    }
    pub(crate) fn height(&self) -> u32 {
        self.height
    }

    #[cfg(test)]
    pub(super) fn pixel_format(&self) -> u32 {
        objc2_core_video::CVPixelBufferGetPixelFormatType(&self.pixel_buffer)
    }
}

impl fmt::Debug for GpuFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GpuFrame")
            .field("pts", &self.pts)
            .field("size", &(self.width, self.height))
            .field("visible_origin", &self.visible_origin)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CpuFormat {
    Nv12,
    P010,
    Ayuv,
    Y410,
}

pub(crate) struct CpuFrame {
    pub pts: i64,
    pub width: u32,
    pub height: u32,
    pub coded_width: u32,
    pub coded_height: u32,
    pub format: CpuFormat,
    pub data: Bytes,
}

pub(crate) enum PlatformDecodedFrame {
    Gpu(GpuFrame),
    Cpu(CpuFrame),
}

pub(crate) struct PlatformVideoDecoder {
    backend: Backend,
    pending: VecDeque<PlatformDecodedFrame>,
    dropped: VecDeque<i64>,
    notification: DecoderNotification,
}

enum Backend {
    Hardware(Box<vt::Hardware>),
    Software(Box<farwing_codec::decoder::Decoder>),
}

impl PlatformVideoDecoder {
    pub(crate) fn open(config: &VideoDecoderConfig) -> Result<Self, DecodeError> {
        if !(1..=16384).contains(&config.width) || !(1..=16384).contains(&config.height) {
            return Err(DecodeError::InvalidInput);
        }
        let backend = match config.mode {
            DecoderMode::Hardware => Backend::Hardware(Box::new(vt::Hardware::open(config)?)),
            DecoderMode::Software => {
                if config.codec != VideoCodec::H264 {
                    return Err(DecodeError::Unsupported);
                }
                let decoder =
                    farwing_codec::decoder::Decoder::new(config.codec, &config.extra_data)
                        .map_err(software_error)?;
                Backend::Software(Box::new(decoder))
            }
        };
        let mut decoder = Self {
            backend,
            pending: VecDeque::new(),
            dropped: VecDeque::new(),
            notification: DecoderNotification::default(),
        };
        decoder.set_notification(decoder.notification.clone());
        Ok(decoder)
    }

    pub(crate) fn probe_format(
        _codec: VideoCodec,
        _width: u32,
        _height: u32,
        _depth: u8,
        _chroma: u8,
    ) -> bool {
        // T2c: advertise only formats verified with hardware fixture decoding.
        false
    }

    pub(crate) fn supports_hardware_format(
        _codec: VideoCodec,
        _format: VideoFormatSignature,
    ) -> bool {
        // T2c: consume the verified hardware capabilities for candidate selection.
        false
    }

    pub(crate) fn set_notification(&mut self, notification: DecoderNotification) {
        if let Backend::Software(decoder) = &mut self.backend {
            decoder.set_cancellation(notification.shared_cancellation());
        }
        self.notification = notification;
    }

    pub(crate) fn reset_for_keyframe(&mut self) -> Result<(), DecodeError> {
        self.pending.clear();
        self.dropped.clear();
        self.check_cancelled()?;
        match &mut self.backend {
            Backend::Hardware(decoder) => {
                decoder.reset();
                Ok(())
            }
            Backend::Software(decoder) => decoder.reset().map_err(software_error),
        }
    }

    pub(crate) fn push_packet(&mut self, data: &[u8], token: i64) -> Result<(), DecodeError> {
        self.check_cancelled()?;
        let output = match &mut self.backend {
            Backend::Hardware(decoder) => decoder
                .push(data, token)
                .map(|frame| frame.map(PlatformDecodedFrame::Gpu)),
            Backend::Software(decoder) => software_push(decoder, data, token),
        };
        self.check_cancelled()?;
        match output {
            Ok(Some(frame)) => self.pending.push_back(frame),
            Ok(None) => self.dropped.push_back(token),
            Err(error) => {
                self.dropped.push_back(token);
                return Err(error);
            }
        }
        Ok(())
    }

    pub(crate) fn poll_owned_frame(&mut self) -> Result<Option<PlatformDecodedFrame>, DecodeError> {
        self.check_cancelled()?;
        Ok(self.pending.pop_front())
    }

    pub(crate) fn poll_dropped_token(&mut self) -> Option<i64> {
        self.check_cancelled().ok()?;
        self.dropped.pop_front()
    }

    fn check_cancelled(&mut self) -> Result<(), DecodeError> {
        if self.notification.is_cancelled() {
            self.pending.clear();
            self.dropped.clear();
            return Err(DecodeError::Closed);
        }
        Ok(())
    }
}

fn software_error(error: farwing_codec::decoder::Error) -> DecodeError {
    use farwing_codec::decoder::Error;
    match error {
        Error::Closed => DecodeError::Closed,
        Error::Unsupported => DecodeError::Unsupported,
        Error::Allocation => DecodeError::Backend,
        Error::InvalidInput => DecodeError::InvalidInput,
        Error::NeedKeyframe => DecodeError::NeedKeyframe,
    }
}

fn software_push(
    decoder: &mut farwing_codec::decoder::Decoder,
    data: &[u8],
    token: i64,
) -> Result<Option<PlatformDecodedFrame>, DecodeError> {
    use farwing_codec::PixelFormat as F;
    let mut frames = decoder
        .push(data, token)
        .map_err(software_error)?
        .into_iter();
    let Some(frame) = frames.next() else {
        return Ok(None);
    };
    // The H.264 kernel is low-delay and returns at most one picture for this AU.
    if frame.pts != token || frames.next().is_some() {
        return Err(DecodeError::Backend);
    }
    Ok(Some(PlatformDecodedFrame::Cpu(CpuFrame {
        pts: frame.pts,
        width: frame.width,
        height: frame.height,
        coded_width: frame.coded_width,
        coded_height: frame.coded_height,
        data: frame.data,
        format: match frame.format {
            F::Nv12 => CpuFormat::Nv12,
            F::P010 => CpuFormat::P010,
            F::Ayuv => CpuFormat::Ayuv,
            F::Y410 => CpuFormat::Y410,
            F::Bgra => return Err(DecodeError::Unsupported),
        },
    })))
}
