use crate::media::decode_api::{DecodeError, DecoderMode, VideoDecoderConfig};
use crate::media::video_format::VideoFormatSignature;
use crate::media::{ConnectionMediaProfile, VideoCodec};
use crate::platform::{
    decoder::{PlatformDecodedFrame, PlatformVideoDecoder},
    surface::{GpuDevice, Surface, SurfaceWriter},
};
use crate::transport::rtc::EncodedVideoFrame;
use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use farwing_protocol::session::Decoder;

/// The sizes a hardware decoder is probed at, largest first.
pub(crate) const PROBE_SIZES: [(u32, u32); 3] = [(3840, 2160), (2560, 1440), (1920, 1080)];

#[cfg(windows)]
fn session_codec(codec: VideoCodec) -> farwing_protocol::session::Codec {
    match codec {
        VideoCodec::H264 => farwing_protocol::session::Codec::H264,
        VideoCodec::H265 => farwing_protocol::session::Codec::Hevc,
        VideoCodec::Av1 => farwing_protocol::session::Codec::Av1,
    }
}

#[cfg(windows)]
pub(crate) mod diagnostics;
pub(crate) mod software_slot;

#[derive(Debug)]
pub(crate) struct DecodedFrame {
    pub pts: i64,
    pub width: u32,
    pub height: u32,
    pub surface: Surface,
    pub ready_at: std::time::Instant,
}

#[derive(Default)]
pub(crate) struct DecodedBatch {
    pub frames: Vec<DecodedFrame>,
    pub input_error: Option<anyhow::Error>,
    pub output_issues: Vec<DecoderOutputIssue>,
}

pub(crate) enum DecoderOutputIssue {
    Dropped(i64),
    Failed {
        token: Option<i64>,
        error: anyhow::Error,
    },
}

/// What the presenter draws: a native surface, prepared on the decoder thread.
pub(crate) type RenderSurface = Surface;

pub(crate) struct NativeVideoDecoder {
    decoder: Box<PlatformVideoDecoder>,
    /// Wraps hardware output and uploads software pictures.
    writer: SurfaceWriter,
    candidate: DecoderCandidate,
    label: String,

    software_slot: Option<std::rc::Rc<software_slot::SoftwareSlot>>,
}

/// Local implementation choices, never serialized as invented decoder IDs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DecoderCandidate {
    #[cfg(windows)]
    WindowsD3d11 {
        adapter: u64,
    },
    #[cfg(target_os = "macos")]
    VideoToolbox,
    Software,
}

impl DecoderCandidate {
    pub(crate) fn preferred(
        codec: VideoCodec,
        preference: crate::media::selection::DecoderPreference,
        preferred: Option<&SurfaceWriter>,
    ) -> Vec<Self> {
        let mut candidates = Vec::new();
        if preference.mode.hardware() {
            #[cfg(windows)]
            {
                if let Some(writer) = preferred {
                    candidates.push(Self::WindowsD3d11 {
                        adapter: writer.adapter_id(),
                    });
                }
                match SurfaceWriter::adapter_ids() {
                    Ok(adapters) => {
                        for adapter in adapters {
                            let candidate = Self::WindowsD3d11 { adapter };
                            if !candidates.contains(&candidate) {
                                candidates.push(candidate);
                            }
                        }
                    }
                    Err(error) => tracing::warn!(%error, "decoder adapter enumeration failed"),
                }
            }
            #[cfg(target_os = "macos")]
            {
                // VideoToolbox selects its own device, independent of the writer.
                let _ = preferred;
                candidates.push(Self::VideoToolbox);
            }
        }

        #[cfg(windows)]
        if let Some(adapter) = crate::media::selection::resolve_gpu(preference.gpu) {
            candidates.sort_by_key(|candidate| *candidate != Self::WindowsD3d11 { adapter });
        }
        #[cfg(windows)]
        let software_codec = matches!(codec, VideoCodec::H264 | VideoCodec::Av1);
        #[cfg(target_os = "macos")]
        let software_codec = codec == VideoCodec::H264;
        if preference.mode.software() && software_codec {
            candidates.push(Self::Software);
        }

        candidates
    }

    pub(crate) fn supports(self, codec: VideoCodec, format: VideoFormatSignature) -> bool {
        match self {
            #[cfg(windows)]
            Self::WindowsD3d11 { .. } => {
                (format.chroma_format_idc == 1
                    || matches!(codec, VideoCodec::H265 | VideoCodec::Av1)
                        && format.chroma_format_idc == 3)
                    && format.bit_depth_luma == format.bit_depth_chroma
                    && (format.bit_depth_luma == 8
                        || matches!(codec, VideoCodec::H265 | VideoCodec::Av1)
                            && format.bit_depth_luma == 10)
            }
            #[cfg(target_os = "macos")]
            Self::VideoToolbox => PlatformVideoDecoder::supports_hardware_format(codec, format),
            Self::Software => {
                farwing_codec::Format {
                    codec,
                    chroma: format.chroma_format_idc,
                    depth: format.bit_depth_luma,
                }
                .can_decode()
                    && format.bit_depth_luma == format.bit_depth_chroma
            }
        }
    }
}

impl NativeVideoDecoder {
    pub(crate) fn open(
        codec: VideoCodec,
        width: u32,
        height: u32,
        frame_rate: u32,
        preference: crate::media::selection::DecoderPreference,
        extra_data: Bytes,
    ) -> Result<Self> {
        Self::open_inner(
            codec, width, height, frame_rate, preference, extra_data, None,
        )
    }

    pub(crate) fn open_with_surface_writer(
        codec: VideoCodec,
        width: u32,
        height: u32,
        frame_rate: u32,
        preference: crate::media::selection::DecoderPreference,
        extra_data: Bytes,
        surface_writer: SurfaceWriter,
    ) -> Result<Self> {
        Self::open_inner(
            codec,
            width,
            height,
            frame_rate,
            preference,
            extra_data,
            Some(surface_writer),
        )
    }

    fn open_inner(
        codec: VideoCodec,
        width: u32,
        height: u32,
        frame_rate: u32,
        preference: crate::media::selection::DecoderPreference,
        extra_data: Bytes,
        surface_writer: Option<SurfaceWriter>,
    ) -> Result<Self> {
        let mut last_error = None;
        for candidate in DecoderCandidate::preferred(codec, preference, surface_writer.as_ref()) {
            match Self::open_candidate(
                candidate,
                codec,
                width,
                height,
                frame_rate,
                extra_data.clone(),
                surface_writer.clone(),
                None,
            ) {
                Ok(mut decoder) => {
                    decoder.describe_selection(preference, last_error.is_some());
                    return Ok(decoder);
                }
                Err(error) => {
                    tracing::debug!(?candidate, %error, "native decoder candidate unavailable");
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| DecodeError::NoBackend.into()))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open_candidate(
        candidate: DecoderCandidate,
        codec: VideoCodec,
        width: u32,
        height: u32,
        frame_rate: u32,
        extra_data: Bytes,
        surface_writer: Option<SurfaceWriter>,
        software_slot: Option<std::rc::Rc<software_slot::SoftwareSlot>>,
    ) -> Result<Self> {
        if width == 0 || height == 0 || frame_rate == 0 {
            return Err(DecodeError::InvalidInput.into());
        }
        let format = crate::media::video_format::parse_stream_format(codec, &extra_data);
        let depth = format.map_or(8, |f| f.bit_depth_luma);
        let chroma = format.map_or(1, |f| f.chroma_format_idc);
        match candidate {
            #[cfg(windows)]
            DecoderCandidate::WindowsD3d11 { adapter } => {
                let supports = |writer: &SurfaceWriter| {
                    PlatformVideoDecoder::probe_format(
                        writer.device_handle(),
                        codec,
                        width,
                        height,
                        depth,
                        chroma,
                    )
                };
                let writer = match surface_writer.filter(|writer| {
                    writer.adapter_id() == adapter
                        && unsafe { writer.device_handle().GetDeviceRemovedReason().is_ok() }
                }) {
                    Some(writer) => writer,
                    None => SurfaceWriter::for_adapter(adapter)?,
                };
                if !supports(&writer) {
                    return Err(DecodeError::Unsupported.into());
                }
                Self::open_windows_hardware(codec, width, height, writer, extra_data)
            }
            #[cfg(target_os = "macos")]
            DecoderCandidate::VideoToolbox => {
                let config = decoder_config(
                    codec,
                    width,
                    height,
                    DecoderMode::Hardware,
                    None,
                    extra_data,
                );
                let decoder = PlatformVideoDecoder::open(&config)?;
                let writer = surface_writer.map_or_else(SurfaceWriter::new, Ok)?;
                Ok(Self {
                    decoder: Box::new(decoder),
                    writer,
                    candidate,
                    label: (crate::i18n::t().session.decoder_hardware)(platform_label()),
                    software_slot: None,
                })
            }
            DecoderCandidate::Software => {
                if !(farwing_codec::Format {
                    codec,
                    chroma,
                    depth,
                })
                .can_decode()
                {
                    return Err(DecodeError::Unsupported.into());
                }
                let software_slot =
                    Some(software_slot.map_or_else(software_slot::SoftwareSlot::acquire, Ok)?);
                let config = decoder_config(
                    codec,
                    width,
                    height,
                    DecoderMode::Software,
                    None,
                    extra_data,
                );
                let decoder = PlatformVideoDecoder::open(&config)?;
                let writer = surface_writer.map_or_else(SurfaceWriter::new, Ok)?;
                Ok(Self {
                    decoder: Box::new(decoder),
                    writer,
                    candidate,
                    label: crate::i18n::t().session.decoder_software.to_owned(),

                    software_slot,
                })
            }
        }
    }

    pub(crate) const fn candidate(&self) -> DecoderCandidate {
        self.candidate
    }
    pub(crate) fn describe_selection(
        &mut self,
        preference: crate::media::selection::DecoderPreference,
        recovered: bool,
    ) {
        let note = if recovered {
            Some(crate::i18n::t().session.decoder_note_recovered)
        } else {
            #[cfg(windows)]
            {
                self.preferred_gpu_note(preference)
            }
            #[cfg(target_os = "macos")]
            {
                None
            }
        };
        if let Some(note) = note {
            self.label
                .push_str(&(crate::i18n::t().session.decoder_note)(note));
        }
        tracing::info!(preference=?preference, actual=?self.candidate, reason=note.unwrap_or("selected"), "decoder preference resolved");
    }

    #[cfg(windows)]
    fn preferred_gpu_note(
        &self,
        preference: crate::media::selection::DecoderPreference,
    ) -> Option<&'static str> {
        use crate::media::selection::{ProcessingMode, resolve_gpu};
        if preference.mode == ProcessingMode::Software {
            None
        } else if let Some(id) = preference.gpu {
            match resolve_gpu(Some(id)) {
                None => Some(crate::i18n::t().session.decoder_note_missing_gpu),
                Some(adapter) if self.candidate != (DecoderCandidate::WindowsD3d11 { adapter }) => {
                    Some(crate::i18n::t().session.selection_gpu_format)
                }
                _ => Some(crate::i18n::t().session.decoder_note_preferred_gpu),
            }
        } else {
            None
        }
    }

    pub(crate) fn is_software(&self) -> bool {
        self.software_slot.is_some()
    }

    pub(crate) fn software_slot(&self) -> Option<std::rc::Rc<software_slot::SoftwareSlot>> {
        self.software_slot.clone()
    }

    pub(crate) fn surface_writer(&self) -> Option<SurfaceWriter> {
        Some(self.writer.clone())
    }

    #[cfg(windows)]
    fn open_windows_hardware(
        codec: VideoCodec,
        width: u32,
        height: u32,
        reader: SurfaceWriter,
        extra_data: Bytes,
    ) -> Result<Self> {
        let config = decoder_config(
            codec,
            width,
            height,
            DecoderMode::Hardware,
            Some(reader.device_handle()),
            extra_data,
        );
        let backend = PlatformVideoDecoder::open(&config)?;
        let adapter = reader.adapter_id();
        tracing::info!(
            adapter,
            ?codec,
            width,
            height,
            "opened hardware decoder on adapter"
        );
        Ok(Self {
            decoder: Box::new(backend),
            writer: reader,
            candidate: DecoderCandidate::WindowsD3d11 { adapter },
            label: (crate::i18n::t().session.decoder_hardware)(
                &crate::platform::capture::encoding_adapters()
                    .ok()
                    .and_then(|items| items.into_iter().find(|a| a.luid == adapter))
                    .map_or_else(|| platform_label().into(), |a| a.name),
            ),
            software_slot: None,
        })
    }

    pub(crate) fn label(&self) -> &str {
        &self.label
    }

    pub(crate) fn set_notification(
        &mut self,
        notification: crate::media::decode_api::DecoderNotification,
    ) {
        self.decoder.set_notification(notification);
    }

    pub(crate) fn reset_for_keyframe(&mut self) -> Result<()> {
        self.decoder
            .reset_for_keyframe()
            .with_context(|| format!("reset {} decoder for keyframe cutover", self.label))
    }

    pub(crate) fn push(&mut self, frame: EncodedVideoFrame, decode_token: i64) -> DecodedBatch {
        let input_error = self
            .decoder
            .push_packet(&frame.data, decode_token)
            .context("submit encoded video frame to native decoder")
            .err();
        let mut batch = poll_platform_decoder(&mut self.decoder, &self.writer);
        batch.input_error = input_error;
        batch
    }

    pub(crate) fn poll(&mut self) -> DecodedBatch {
        poll_platform_decoder(&mut self.decoder, &self.writer)
    }
}

fn poll_platform_decoder(
    decoder: &mut PlatformVideoDecoder,
    writer: &SurfaceWriter,
) -> DecodedBatch {
    let mut batch = DecodedBatch::default();
    #[cfg(windows)]
    let context = "poll owning Windows decoded frame";
    #[cfg(target_os = "macos")]
    let context = "poll owning macOS decoded frame";
    loop {
        let frame = match decoder.poll_owned_frame().context(context) {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(error) => {
                batch
                    .output_issues
                    .push(DecoderOutputIssue::Failed { token: None, error });
                break;
            }
        };
        let ready_at = std::time::Instant::now();
        let (pts, width, height, surface) = match frame {
            PlatformDecodedFrame::Gpu(frame) => (
                frame.pts(),
                frame.width(),
                frame.height(),
                writer.wrap_decoded_surface(frame),
            ),
            PlatformDecodedFrame::Cpu(frame) => (
                frame.pts,
                frame.width,
                frame.height,
                writer.upload_cpu(&frame),
            ),
        };
        match surface {
            Ok(surface) => batch.frames.push(DecodedFrame {
                pts,
                width,
                height,
                surface,
                ready_at,
            }),
            Err(error) => batch.output_issues.push(DecoderOutputIssue::Failed {
                token: Some(pts),
                error,
            }),
        }
    }
    while let Some(token) = decoder.poll_dropped_token() {
        batch.output_issues.push(DecoderOutputIssue::Dropped(token));
    }
    batch
}

/// Native decoding capabilities, probed before a connection needs them.
/// On Windows, invalidate when the set of adapters changes (a driver update
/// restarts an adapter under a new LUID).
#[derive(Default)]
struct DecoderInventory {
    #[cfg(windows)]
    adapters: Vec<u64>,
    /// Every codec, in advertising order.
    hardware: Option<Vec<(VideoCodec, Decoder)>>,
    #[cfg(windows)]
    software_av1: Option<Vec<Decoder>>,
}

static DECODER_INVENTORY: std::sync::Mutex<Option<DecoderInventory>> = std::sync::Mutex::new(None);

/// Probe in the background the hardware decoders a connection with this
/// decoding mode will advertise. (Software AV1 support is only probed when a
/// connection accepts AV1, as before.)
pub(crate) fn prepare_native_decoder_support(
    mode: crate::media::selection::ProcessingMode,
    delay: std::time::Duration,
) {
    if !mode.hardware() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("Decoder probe".to_owned())
        .spawn(move || {
            std::thread::sleep(delay);
            let started = std::time::Instant::now();
            decoder_inventory(
                true,
                #[cfg(windows)]
                false,
                |_| (),
            );
            tracing::debug!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                "native decoder capabilities probed in advance"
            );
        });
    if let Err(error) = spawned {
        tracing::debug!(%error, "decoder probe thread unavailable");
    }
}

fn decoder_inventory<T>(
    hardware: bool,
    #[cfg(windows)] software_av1: bool,
    read: impl FnOnce(&DecoderInventory) -> T,
) -> T {
    #[cfg(windows)]
    let adapters = SurfaceWriter::adapter_ids().unwrap_or_default();
    // A connection waits here for a probe already running.
    let mut cached = DECODER_INVENTORY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    #[cfg(windows)]
    if cached
        .as_ref()
        .is_none_or(|inventory| inventory.adapters != adapters)
    {
        *cached = Some(DecoderInventory {
            adapters,
            ..Default::default()
        });
    }
    #[cfg(windows)]
    let inventory = cached.as_mut().expect("decoder inventory");
    #[cfg(target_os = "macos")]
    let inventory = cached.get_or_insert_with(DecoderInventory::default);
    if hardware && inventory.hardware.is_none() {
        inventory.hardware = Some(probe_hardware_decoders());
    }
    #[cfg(windows)]
    if software_av1 && inventory.software_av1.is_none() {
        inventory.software_av1 = Some(probe_software_av1());
    }
    let value = read(inventory);
    // Nothing found may be a passing driver state: probe again next time.
    if inventory.hardware.as_ref().is_some_and(Vec::is_empty) {
        inventory.hardware = None;
    }
    #[cfg(windows)]
    if inventory.software_av1.as_ref().is_some_and(Vec::is_empty) {
        inventory.software_av1 = None;
    }
    value
}

#[cfg(windows)]
fn probe_hardware_decoders() -> Vec<(VideoCodec, Decoder)> {
    let mut capabilities = Vec::new();
    let Ok(writers) = SurfaceWriter::available() else {
        return capabilities;
    };
    for codec in [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1] {
        for chroma in [1, 3] {
            for depth in [8, 10] {
                for (width, height) in PROBE_SIZES {
                    if writers.iter().any(|writer| {
                        PlatformVideoDecoder::probe_format(
                            writer.device_handle(),
                            codec,
                            width,
                            height,
                            depth,
                            chroma,
                        )
                    }) {
                        capabilities.push((
                            codec,
                            Decoder {
                                codec: session_codec(codec),
                                chroma444: chroma == 3,
                                bit10: depth == 10,
                                width,
                                height,
                                hardware: true,
                                av1_visible_size: true,
                            },
                        ));
                        break;
                    }
                }
            }
        }
    }
    capabilities
}

#[cfg(target_os = "macos")]
fn probe_hardware_decoders() -> Vec<(VideoCodec, Decoder)> {
    PlatformVideoDecoder::probe_hardware(&PROBE_SIZES)
}

#[cfg(windows)]
fn probe_software_av1() -> Vec<Decoder> {
    let mut capabilities = Vec::new();
    if let Ok(writer) = SurfaceWriter::new() {
        for chroma in [1, 3] {
            for depth in [8, 10] {
                if writer.supports_software_av1(depth, chroma) {
                    capabilities.push(Decoder {
                        codec: farwing_protocol::session::Codec::Av1,
                        chroma444: chroma == 3,
                        bit10: depth == 10,
                        width: 1920,
                        height: 1080,
                        hardware: false,
                        av1_visible_size: true,
                    });
                }
            }
        }
    }
    capabilities
}

/// The formats this controller decodes, as the session offers them.
pub(crate) fn detect_native_decoder_support(
    profile: ConnectionMediaProfile,
) -> Result<Vec<Decoder>> {
    std::thread::spawn(move || {
        #[cfg(windows)]
        let software_av1 =
            profile.decoder.mode.software() && profile.codec.accepts(VideoCodec::Av1);
        let mut capabilities = Vec::new();
        decoder_inventory(
            profile.decoder.mode.hardware(),
            #[cfg(windows)]
            software_av1,
            |inventory| {
                if profile.decoder.mode.hardware() {
                    capabilities.extend(
                        inventory
                            .hardware
                            .iter()
                            .flatten()
                            .filter(|(codec, _)| profile.codec.accepts(*codec))
                            .map(|(_, capability)| capability.clone()),
                    );
                }
                if profile.decoder.mode.software() && profile.codec.accepts(VideoCodec::H264) {
                    // This is the advertised software ceiling, not
                    // an arbitrary decoder rejection of a larger hardware-fallback AU.
                    for chroma444 in [false, true] {
                        capabilities.push(Decoder {
                            codec: farwing_protocol::session::Codec::H264,
                            chroma444,
                            bit10: false,
                            width: 1920,
                            height: 1080,
                            hardware: false,
                            av1_visible_size: true,
                        });
                    }
                }
                #[cfg(windows)]
                if software_av1 {
                    capabilities.extend(inventory.software_av1.iter().flatten().cloned());
                }
            },
        );

        if capabilities.is_empty() {
            bail!("no decoder supports the selected codec and decoding mode");
        }
        Ok(capabilities)
    })
    .join()
    .map_err(|_| anyhow!("native capability probe thread panicked"))?
}

fn decoder_config(
    codec: VideoCodec,
    width: u32,
    height: u32,
    mode: DecoderMode,
    gpu_device: Option<GpuDevice>,
    extra_data: Bytes,
) -> VideoDecoderConfig {
    VideoDecoderConfig {
        codec,
        width,
        height,
        mode,
        gpu_device,
        extra_data,
    }
}

#[cfg(windows)]
fn platform_label() -> &'static str {
    "DXVA11"
}

#[cfg(target_os = "macos")]
fn platform_label() -> &'static str {
    "VideoToolbox"
}
