//! VideoToolbox with a single synchronous submission in flight.

use std::{
    ffi::c_void,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr::{self, NonNull},
    sync::{Arc, Mutex},
};

use objc2_core_foundation::{
    CFBoolean, CFDictionary, CFGetTypeID, CFNumber, CFRetained, CFString, CFType,
};
use objc2_core_media::{
    CMTime, CMVideoFormatDescription, kCMFormatDescriptionError_AllocationFailed,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBufferGetHeight, CVPixelBufferGetIOSurface,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount, CVPixelBufferGetTypeID,
    CVPixelBufferGetWidth, kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey,
};
use objc2_video_toolbox::*;

use crate::media::{
    decode_api::{DecodeError, VideoDecoderConfig},
    video_format::VideoFormatSignature,
};

use super::{
    GpuFrame,
    bitstream::{Bitstream, Configuration},
    format::{self, PixelFormat},
};

#[derive(Debug, thiserror::Error)]
pub(super) enum HardwareError {
    #[error(transparent)]
    Decode(#[from] DecodeError),
    #[error("VideoToolbox {operation} (OSStatus {status}): {decode}")]
    VtStatus {
        operation: &'static str,
        status: i32,
        decode: DecodeError,
    },
    #[error("{0}")]
    InvalidOutput(String),
}

impl HardwareError {
    pub(super) fn rejects_format(&self) -> bool {
        match self {
            Self::Decode(DecodeError::Unsupported) => true,
            Self::VtStatus { status, decode, .. } => {
                *decode == DecodeError::Unsupported
                    || session_rejects_format(*status)
                    || *status == kVTFormatDescriptionChangeNotSupportedErr
            }
            _ => false,
        }
    }
}

// Keep the playback recovery contract while giving the probe the native reason.
impl From<HardwareError> for DecodeError {
    fn from(error: HardwareError) -> Self {
        match error {
            HardwareError::Decode(error) | HardwareError::VtStatus { decode: error, .. } => error,
            HardwareError::InvalidOutput(_) => Self::HardwareFailure,
        }
    }
}

pub(super) struct Hardware {
    stream: Bitstream,
    installed: Option<Installed>,
    need_keyframe: bool,
    leading: HevcLeading,
    #[cfg(test)]
    pub(super) session_creations: usize,
}

struct Installed {
    configuration: Configuration,
    description: CFRetained<CMVideoFormatDescription>,
    session: Session,
}

impl Hardware {
    pub(super) fn open(config: &VideoDecoderConfig) -> Result<Self, DecodeError> {
        let stream = Bitstream::new(config.codec, &config.extra_data)?;
        let installed = stream
            .configuration()
            .map(|config| Installed::new(config, true))
            .transpose()?;
        Ok(Self {
            stream,
            #[cfg(test)]
            session_creations: usize::from(installed.is_some()),
            installed,
            need_keyframe: true,
            leading: HevcLeading::default(),
        })
    }

    pub(super) fn reset(&mut self) {
        // A new session discards reference pictures; keep verified parameter sets.
        self.installed = None;
        self.need_keyframe = true;
        self.leading = HevcLeading::default();
    }

    pub(super) fn push(&mut self, data: &[u8], token: i64) -> Output {
        let result = self.decode(data, token);
        if result.is_err() && !self.need_keyframe {
            // decode drains any failed submit before clearing its active slot.
            // If the wait itself failed, invalidating here still prevents a
            // late callback from matching a reused stack cookie on this session.
            self.reset();
        }
        result
    }

    fn decode(&mut self, data: &[u8], token: i64) -> Output {
        let sample = self.stream.prepare(data).map_err(DecodeError::from)?;
        if !sample.has_picture {
            return Ok(None);
        }
        if self.need_keyframe && !sample.is_keyframe {
            // AV1 uses the sender's in-band sequence header + shown keyframe.
            // NVENC repeats the header, AMF inserts it at keyframes, and the
            // oneVPL AV1 packer emits it for I pictures. Extradata can seed
            // the session but does not make a headerless AV1 AU a sync point.
            return Err(DecodeError::NeedKeyframe.into());
        }
        if self.leading.discard(sample.hevc_vcl_nal_type) {
            return Ok(None);
        }
        let config = self
            .stream
            .configuration()
            .ok_or(DecodeError::NeedKeyframe)?;
        let mut random_access = self.need_keyframe;
        // Compare against the installed configuration. A parameter-only packet
        // may have changed the stream's configuration before this picture arrived.
        if self
            .installed
            .as_ref()
            .is_none_or(|old| old.configuration != *config)
        {
            let opening = self.installed.is_none();
            let (description, pixel) = prepare_format(config, opening)?;
            if let Some(old) = self
                .installed
                .as_mut()
                .filter(|old| old.session.accepts(&description, pixel))
            {
                old.configuration = config.clone();
                old.description = description;
            } else {
                if !sample.is_keyframe {
                    return Err(DecodeError::NeedKeyframe.into());
                }
                self.installed = None;
                self.installed = Some(Installed::create(config, description, pixel, opening)?);
                tracing::debug!(
                    in_band_change = sample.configuration_changed,
                    "installed VideoToolbox decoder session"
                );
                #[cfg(test)]
                {
                    self.session_creations += 1;
                }
                random_access = true;
            }
        }
        self.leading
            .observe(sample.hevc_vcl_nal_type, random_access);
        let installed = self
            .installed
            .as_mut()
            .ok_or(DecodeError::HardwareFailure)?;
        self.need_keyframe = false; // On a submit error, push() invalidates the reference chain.
        installed
            .session
            .decode(&sample.data, &installed.description, config.format(), token)
    }
}

#[derive(Default)]
struct HevcLeading(bool);

impl HevcLeading {
    fn discard(&self, kind: Option<u8>) -> bool {
        self.0 && matches!(kind, Some(8 | 9))
    }

    fn observe(&mut self, kind: Option<u8>, random_access: bool) {
        match kind {
            // BLA always has NoRaslOutputFlag. A CRA has it on random access,
            // not when reached in sequence with the previous references intact.
            Some(16..=18) => self.0 = true,
            Some(21) => self.0 = random_access,
            Some(6..=9) => {} // RADL/RASL: preserve the associated IRAP state.
            _ => self.0 = false,
        }
    }
}

impl Installed {
    fn new(config: &Configuration, opening: bool) -> Result<Self, HardwareError> {
        let (description, pixel) = prepare_format(config, opening)?;
        Self::create(config, description, pixel, opening)
    }

    fn create(
        config: &Configuration,
        description: CFRetained<CMVideoFormatDescription>,
        pixel: PixelFormat,
        opening: bool,
    ) -> Result<Self, HardwareError> {
        Ok(Self {
            session: Session::new(&description, pixel, opening)?,
            configuration: config.clone(),
            description,
        })
    }
}

fn prepare_format(
    config: &Configuration,
    opening: bool,
) -> Result<(CFRetained<CMVideoFormatDescription>, PixelFormat), DecodeError> {
    let pixel = PixelFormat::for_configuration(config).ok_or(if opening {
        DecodeError::Unsupported
    } else {
        DecodeError::HardwareFailure
    })?;
    let description = format::description(config).map_err(|status| {
        tracing::debug!(status, "CoreMedia rejected the video format description");
        if status == kCMFormatDescriptionError_AllocationFailed {
            DecodeError::HardwareFailure
        } else if opening {
            DecodeError::Unsupported
        } else {
            DecodeError::NeedKeyframe
        }
    })?;
    Ok((description, pixel))
}

struct Session {
    session: CFRetained<VTDecompressionSession>,
    callback: Arc<Callback>,
}

type Output = Result<Option<GpuFrame>, HardwareError>;

struct Callback {
    pixel: PixelFormat,
    active: Mutex<Option<Submission>>,
    #[cfg(test)]
    panic_on_output: bool,
}

struct Submission {
    source_addr: usize,
    token: i64,
    signature: VideoFormatSignature,
    output: Option<Output>,
}

impl Session {
    fn new(
        description: &CMVideoFormatDescription,
        pixel: PixelFormat,
        opening: bool,
    ) -> Result<Self, HardwareError> {
        // SAFETY: Framework-owned immutable CFString constants.
        let (require_hardware, format_key, surface_key) = unsafe {
            (
                kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder,
                kCVPixelBufferPixelFormatTypeKey,
                kCVPixelBufferIOSurfacePropertiesKey,
            )
        };
        let spec = CFDictionary::from_slices(&[require_hardware], &[CFBoolean::new(true)]);
        let pixel_type = CFNumber::new_i32(pixel.fourcc as i32);
        let iosurface = CFDictionary::<CFString, CFType>::empty();
        let destination = CFDictionary::<CFString, CFType>::from_slices(
            &[format_key, surface_key],
            &[pixel_type.as_ref(), iosurface.as_opaque().as_ref()],
        );
        let callback = Arc::new(Callback {
            pixel,
            active: Mutex::new(None),
            #[cfg(test)]
            panic_on_output: false,
        });
        let record = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(output_callback),
            decompressionOutputRefCon: Arc::as_ptr(&callback).cast_mut().cast(),
        };
        let mut out = ptr::null_mut();
        // SAFETY: Correctly typed CF dictionaries, copied callback record and
        // a refcon borrowed from the Arc until session invalidation.
        let status = unsafe {
            VTDecompressionSession::create(
                None,
                description,
                Some(spec.as_opaque()),
                Some(destination.as_opaque()),
                &record,
                NonNull::from_mut(&mut out),
            )
        };
        if status != 0 {
            tracing::debug!(status, "VideoToolbox session creation failed");
            #[cfg(test)]
            eprintln!("VT create OSStatus={status}");
            return Err(HardwareError::VtStatus {
                operation: "session creation",
                status,
                decode: if opening && session_rejects_format(status) {
                    DecodeError::Unsupported
                } else {
                    DecodeError::HardwareFailure
                },
            });
        }
        let out = NonNull::new(out).ok_or(DecodeError::HardwareFailure)?;
        // SAFETY: A successful Create returns a +1 session.
        let session = unsafe { CFRetained::from_raw(out) };
        let this = Self { session, callback };
        // Production trusts RequireHardwareAcceleratedVideoDecoder. Tests verify
        // that contract independently without a property query on each live open.
        #[cfg(test)]
        if !this.uses_hardware() {
            return Err(HardwareError::InvalidOutput(
                "VideoToolbox did not confirm hardware decode".into(),
            ));
        }
        Ok(this)
    }

    fn accepts(&self, description: &CMVideoFormatDescription, pixel: PixelFormat) -> bool {
        // SAFETY: Both objects are retained. The destination attributes of an
        // existing session cannot be changed by accepting a new description.
        (unsafe { self.session.can_accept_format_description(description) })
            && self.callback.pixel == pixel
    }

    #[cfg(test)]
    fn uses_hardware(&self) -> bool {
        let mut out: *mut CFType = ptr::null_mut();
        // SAFETY: A live VT session and a writable output for the CFTypeRef.
        let status = unsafe {
            VTSessionCopyProperty(
                self.session.as_ref(),
                kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder,
                None,
                ptr::from_mut(&mut out).cast(),
            )
        };
        // SAFETY: A non-null Copy result is retained (+1).
        status == 0
            && NonNull::new(out).is_some_and(|out| {
                unsafe { CFRetained::from_raw(out) }
                    .downcast::<CFBoolean>()
                    .is_ok_and(|value| value.as_bool())
            })
    }

    fn decode(
        &mut self,
        data: &[u8],
        description: &CMVideoFormatDescription,
        signature: VideoFormatSignature,
        mut token: i64,
    ) -> Output {
        let sample = format::sample_buffer(data, description).map_err(|status| {
            tracing::debug!(status, "could not construct a CoreMedia sample");
            DecodeError::HardwareFailure
        })?;
        // Pass an address, not the integer as a pointer: zero is a valid token.
        // The callback compares this cookie but does not dereference it.
        let source = ptr::from_mut(&mut token).cast::<c_void>();
        *self
            .callback
            .active
            .lock()
            .map_err(|_| DecodeError::HardwareFailure)? = Some(Submission {
            source_addr: source.addr(),
            token,
            signature,
            output: None,
        });
        let mut info = VTDecodeInfoFlags::empty();
        // SAFETY: All objects and refcons are live. With both asynchronous and
        // temporal processing disabled, VT promises the callback before return.
        let status = unsafe {
            self.session
                .decode_frame(&sample, VTDecodeFrameFlags::empty(), source, &mut info)
        };
        let mut active = self
            .callback
            .active
            .lock()
            .map_err(|_| DecodeError::HardwareFailure)?;
        let dropped = info.contains(VTDecodeInfoFlags::FrameDropped);
        let mut completion = status;
        let delayed = status == 0
            && (info.contains(VTDecodeInfoFlags::Asynchronous)
                || (!dropped && active.as_ref().is_some_and(|entry| entry.output.is_none())));
        if status != 0 || delayed {
            drop(active); // The callback must not contend with a held lock during the wait.
            if delayed {
                tracing::warn!(
                    token,
                    ?info,
                    "VideoToolbox did not complete synchronous decode inline"
                );
                #[cfg(test)]
                eprintln!("VT delayed callback for token {token}; draining before return");
            }
            // SAFETY: The session and refcons remain live, including for a failed submit.
            // The wait also finishes delayed frames. Preserve the original submit error.
            let wait_status = unsafe { self.session.wait_for_asynchronous_frames() };
            if status == 0 {
                completion = wait_status;
            }
            active = self
                .callback
                .active
                .lock()
                .map_err(|_| DecodeError::HardwareFailure)?;
        }
        let output = active.take().and_then(|entry| entry.output);
        if completion != 0 {
            Err(decode_failure(completion))
        } else {
            output.unwrap_or_else(|| {
                if dropped {
                    Ok(None)
                } else {
                    Err(HardwareError::InvalidOutput(
                        "VideoToolbox produced no output".into(),
                    ))
                }
            })
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: Both the session and the refcon Arc are alive until after
        // the wait and invalidate; no more callbacks can use the refcon then.
        unsafe {
            let _ = self.session.wait_for_asynchronous_frames();
            self.session.invalidate();
        }
    }
}

fn session_rejects_format(status: i32) -> bool {
    [
        kVTCouldNotFindVideoDecoderErr,
        kVTVideoDecoderUnsupportedDataFormatErr,
        kVTPixelTransferNotSupportedErr,
    ]
    .contains(&status)
}

fn decode_failure(status: i32) -> HardwareError {
    HardwareError::VtStatus {
        operation: "decode",
        status,
        decode: decode_status(status),
    }
}

fn decode_status(status: i32) -> DecodeError {
    tracing::debug!(status, "VideoToolbox decode failed");
    if [
        kVTVideoDecoderBadDataErr,
        kVTVideoDecoderReferenceMissingErr,
        kVTVideoDecoderUnsupportedDataFormatErr,
        kVTFormatDescriptionChangeNotSupportedErr,
    ]
    .contains(&status)
    {
        DecodeError::NeedKeyframe
    } else {
        // Includes InvalidSession (e.g. after sleep), Malfunction and NotAvailableNow.
        DecodeError::HardwareFailure
    }
}

unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    source: *mut c_void,
    status: i32,
    flags: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    _: CMTime,
    _: CMTime,
) {
    // SAFETY: VT returns the Callback refcon, kept live by Session's Arc.
    let callback = unsafe { &*refcon.cast::<Callback>() };
    let Ok(mut active) = callback.active.lock() else {
        return;
    };
    let Some(entry) = active
        .as_mut()
        .filter(|entry| entry.source_addr == source.addr() && entry.output.is_none())
    else {
        return;
    };
    // Keep the guard outside the unwind boundary so callback work cannot poison it.
    let output = catch_unwind(AssertUnwindSafe(|| {
        #[cfg(test)]
        assert!(!callback.panic_on_output, "injected VT callback panic");
        if status != 0 {
            Err(decode_failure(status))
        } else if flags.contains(VTDecodeInfoFlags::FrameDropped) || image.is_null() {
            Ok(None)
        } else {
            // SAFETY: A non-null image from the callback is live through this call.
            frame_from_callback(
                unsafe { CFRetained::retain(NonNull::new_unchecked(image)) },
                callback.pixel,
                entry,
            )
        }
    }));
    match output {
        Ok(output) => entry.output = Some(output),
        Err(payload) => {
            let token = entry.token;
            entry.output = Some(Err(DecodeError::HardwareFailure.into()));
            drop(active);
            // Even recovery must not unwind across VT: logging and disposing
            // of an arbitrary Rust panic payload can themselves panic.
            if let Err(payload) =
                catch_unwind(|| tracing::error!(token, "VideoToolbox output callback panicked"))
            {
                std::mem::forget(payload);
            }
            if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
                std::mem::forget(payload);
            }
        }
    }
}

fn frame_from_callback(
    pixel_buffer: CFRetained<CVImageBuffer>,
    pixel: PixelFormat,
    entry: &Submission,
) -> Output {
    if CFGetTypeID(Some(pixel_buffer.as_ref())) != CVPixelBufferGetTypeID() {
        return Err(HardwareError::InvalidOutput(
            "VideoToolbox returned no pixel buffer".into(),
        ));
    }
    let fourcc = CVPixelBufferGetPixelFormatType(&pixel_buffer);
    let planes = CVPixelBufferGetPlaneCount(&pixel_buffer);
    if fourcc != pixel.fourcc || planes != 2 {
        tracing::debug!(
            actual = fourcc,
            expected = pixel.fourcc,
            "VideoToolbox returned an unexpected pixel format"
        );
        return Err(HardwareError::InvalidOutput(format!(
            "unexpected pixel format {fourcc:#010x} ({planes} planes), expected {:#010x} (2 planes)",
            pixel.fourcc
        )));
    }
    if CVPixelBufferGetIOSurface(Some(&pixel_buffer)).is_none() {
        return Err(HardwareError::InvalidOutput(
            "GPU frame has no IOSurface".into(),
        ));
    }
    let s = entry.signature;
    let (w, h) = (
        CVPixelBufferGetWidth(&pixel_buffer),
        CVPixelBufferGetHeight(&pixel_buffer),
    );
    // VT may already have applied the codec crop. Apply the bitstream origin
    // only on axes still returned in coded coordinates.
    let visible_offset = |actual: usize, visible: u32, coded: u32, offset: u32| {
        if actual == visible as usize {
            Some(0)
        } else if actual >= coded as usize && actual >= (visible + offset) as usize {
            Some(offset)
        } else {
            None
        }
    };
    let unexpected_dimensions = || {
        HardwareError::InvalidOutput(format!(
            "unexpected pixel buffer dimensions {w}x{h}, expected {}x{} visible or {}x{} coded",
            s.visible_width, s.visible_height, s.coded_width, s.coded_height
        ))
    };
    let x = visible_offset(w, s.visible_width, s.coded_width, s.crop_left)
        .ok_or_else(unexpected_dimensions)?;
    let y = visible_offset(h, s.visible_height, s.coded_height, s.crop_top)
        .ok_or_else(unexpected_dimensions)?;
    Ok(Some(GpuFrame {
        pts: entry.token,
        width: s.visible_width,
        height: s.visible_height,
        visible_origin: (x, y),
        bytes_per_sample: pixel.bytes_per_sample,
        pixel_buffer,
    }))
}

#[test]
fn hevc_rasl_is_only_skipped_at_random_access() {
    let mut leading = HevcLeading::default();
    leading.observe(Some(21), true);
    assert!(leading.discard(Some(8)));
    assert!(!leading.discard(Some(7)));
    leading.observe(Some(7), false);
    assert!(leading.discard(Some(9)));
    leading.observe(Some(1), false);
    assert!(!leading.discard(Some(9)));
    leading.observe(Some(21), false);
    assert!(!leading.discard(Some(9)));
    leading.observe(Some(16), false);
    assert!(leading.discard(Some(9)));
    leading.observe(Some(19), false);
    assert!(!leading.discard(Some(8)));
}

#[test]
fn vt_callback_panic_is_contained_and_fails_the_submission() {
    let token = 7i64;
    let source = ptr::from_ref(&token).cast_mut().cast::<c_void>();
    let codec = crate::media::VideoCodec::H264;
    let data = crate::media::decoder::fixtures::sample(codec, 1, 8, (1280, 720)).unwrap();
    let signature = crate::media::video_format::parse_stream_format(codec, data).unwrap();
    let callback = Callback {
        pixel: PixelFormat {
            fourcc: u32::from_be_bytes(*b"420v"),
            bytes_per_sample: 1,
        },
        active: Mutex::new(Some(Submission {
            source_addr: source.addr(),
            token,
            signature,
            output: None,
        })),
        panic_on_output: true,
    };
    // SAFETY: Invoke the Rust callback directly with its actual refcon/cookie
    // and a null image, without involving a VT C frame if the test fails.
    let unwind = catch_unwind(AssertUnwindSafe(|| unsafe {
        let time = objc2_core_media::kCMTimeInvalid;
        output_callback(
            ptr::from_ref(&callback).cast_mut().cast(),
            source,
            0,
            VTDecodeInfoFlags::FrameDropped,
            ptr::null_mut(),
            time,
            time,
        );
    }));
    assert!(unwind.is_ok(), "panic escaped the VT callback");
    let active = callback.active.lock().expect("callback poisoned the mutex");
    assert!(matches!(
        active.as_ref().unwrap().output,
        Some(Err(HardwareError::Decode(DecodeError::HardwareFailure)))
    ));
}

#[test]
fn vt_errors_are_typed_for_keyframe_request_and_backend_fallback() {
    for status in [
        kVTVideoDecoderBadDataErr,
        kVTVideoDecoderReferenceMissingErr,
        kVTVideoDecoderUnsupportedDataFormatErr,
    ] {
        assert_eq!(
            DecodeError::from(decode_failure(status)),
            DecodeError::NeedKeyframe
        );
    }
    for status in [
        kVTInvalidSessionErr,
        kVTVideoDecoderMalfunctionErr,
        kVTSessionMalfunctionErr,
        kVTVideoDecoderNotAvailableNowErr,
    ] {
        let error = anyhow::Error::new(DecodeError::from(decode_failure(status))).context("submit");
        assert_eq!(
            error.downcast_ref::<DecodeError>(),
            Some(&DecodeError::HardwareFailure)
        );
    }
}
