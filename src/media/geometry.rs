//! Verified sender geometry and tier bitrate-ceiling rules, independent of capture/renderer APIs.
/// Local presentation only. Never feeds capture settings or physical modes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum DisplayAspect {
    #[default]
    Video,
    Display,
    Wide,
    WideTen,
    Standard,
    UltraWide,
}
impl DisplayAspect {
    pub const ALL: [Self; 6] = [
        Self::Video,
        Self::Display,
        Self::Wide,
        Self::WideTen,
        Self::Standard,
        Self::UltraWide,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Video => crate::i18n::t().session.aspect_keep,
            Self::Display => crate::i18n::t().session.aspect_display,
            Self::Wide => crate::i18n::t().session.aspect_16_9,
            Self::WideTen => crate::i18n::t().session.aspect_16_10,
            Self::Standard => crate::i18n::t().session.aspect_4_3,
            Self::UltraWide => crate::i18n::t().session.aspect_21_9,
        }
    }
    pub fn ratio(self, preferred: Option<(u32, u32)>) -> Option<(u32, u32)> {
        match self {
            Self::Video => None,
            Self::Display => preferred.filter(|(w, h)| *w > 0 && *h > 0),
            Self::Wide => Some((16, 9)),
            Self::WideTen => Some((16, 10)),
            Self::Standard => Some((4, 3)),
            Self::UltraWide => Some((21, 9)),
        }
    }
    pub fn size(video: (u32, u32), ratio: Option<(u32, u32)>) -> (u32, u32) {
        match ratio {
            Some((w, h)) if w > 0 && h > 0 => (
                ((u64::from(video.1) * u64::from(w) + u64::from(h) / 2) / u64::from(h))
                    .clamp(1, u64::from(u32::MAX)) as u32,
                video.1,
            ),
            _ => video,
        }
    }
}

/// How a picture is turned: mirrored left to right, then rotated clockwise
/// by `quarter_turns` × 90° (a camera's mirror and rotation, a self view).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Orientation {
    pub mirror: bool,
    pub quarter_turns: u8,
}

impl Orientation {
    /// Width and height trade places.
    pub fn swaps_axes(self) -> bool {
        self.quarter_turns % 2 == 1
    }
    /// The size of a `size` picture once turned.
    pub fn turned(self, size: (u32, u32)) -> (u32, u32) {
        if self.swaps_axes() { (size.1, size.0) } else { size }
    }
    /// Which corner of the source each corner of the turned picture shows.
    /// Corners go clockwise from the top left: 0 top left, 1 top right,
    /// 2 bottom right, 3 bottom left.
    pub fn source_corners(self) -> [usize; 4] {
        let turns = usize::from(self.quarter_turns % 4);
        std::array::from_fn(|corner| {
            let source = (corner + 4 - turns) % 4;
            if self.mirror { [1, 0, 3, 2][source] } else { source }
        })
    }
}

/// A picture size class: the box a source is fitted into.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub(crate) enum Tier {
    P720,
    #[default]
    P1080,
    P1440,
    P2160,
}

impl Tier {
    pub const ALL: [Self; 4] = [Self::P720, Self::P1080, Self::P1440, Self::P2160];

    pub fn size(self) -> (u32, u32) {
        match self {
            Self::P720 => (1280, 720),
            Self::P1080 => (1920, 1080),
            Self::P1440 => (2560, 1440),
            Self::P2160 => (3840, 2160),
        }
    }
    /// The next larger tier.
    pub fn up(self) -> Option<Self> {
        Self::ALL.get(self as usize + 1).copied()
    }
    /// The next smaller tier.
    pub fn down(self) -> Option<Self> {
        (self as usize).checked_sub(1).map(|i| Self::ALL[i])
    }
}

pub(crate) fn output_size(width: u32, height: u32, tier: Tier) -> (u32, u32) {
    fit_size(width, height, tier.size())
}

// 60 FPS ceilings in Mbps at 720p, 1080p, 1440p and 4K. Each codec has
// independent quality anchors; compression savings are content-dependent.
// 10-bit preserves gradients without a blanket raw-bit-depth surcharge.
/// The bitrate ceiling (bps) of `tier` for a `source` screen at `fps`,
/// doubled for 4:4:4 (`true_color`). The quality menus show it.
pub(crate) fn tier_rate(tier: Tier, source: (u32, u32), fps: u32, true_color: bool) -> u32 {
    // One table for every codec: H.265 and AV1 spend their efficiency on
    // picture quality at the same rate, not on a lower rate.
    let rates = match tier {
        Tier::P720 => [1.5, 1.5, 1.5, 1.5],
        Tier::P1080 => [5.5, 8.0, 8.0, 8.0],
        Tier::P1440 => [8.0, 14.0, 24.0, 24.0],
        Tier::P2160 => [12.0, 20.0, 36.0, 60.0],
    };
    let size = output_size(source.0, source.1, tier);
    let pixels = u64::from(size.0) * u64::from(size.1);
    let anchors = [1280u64 * 720, 1920 * 1080, 2560 * 1440, 3840 * 2160];
    let base = if pixels <= anchors[0] {
        rates[0] * (pixels as f64 / anchors[0] as f64).max(0.25)
    } else {
        (1..anchors.len())
            .find(|&i| pixels <= anchors[i])
            .map_or(rates[3], |i| {
                let fraction =
                    (pixels - anchors[i - 1]) as f64 / (anchors[i] - anchors[i - 1]) as f64;
                rates[i - 1] + (rates[i] - rates[i - 1]) * fraction
            })
    };
    // Like Moonlight's default-rate curve, high FPS grows sublinearly: adjacent
    // frames share more information. No 30/60/90/120 tier discontinuities.
    let ratio = f64::from(fps.clamp(1, 144)) / 60.0;
    let color = if true_color { 2.0 } else { 1.0 };
    (base * 1_000_000.0 * color * if fps <= 60 { ratio } else { ratio.sqrt() })
        .round()
        .clamp(500_000.0, 500_000_000.0) as u32
}

/// Whether `size` fits a size budget in either orientation.
pub(crate) fn fits_within(size: (u32, u32), maximum: (u32, u32)) -> bool {
    let (long, short) = (size.0.max(size.1), size.0.min(size.1));
    long <= maximum.0.max(maximum.1) && short <= maximum.0.min(maximum.1)
}

/// Fit a source into a size budget. The budget is orientation-free: a
/// portrait (rotated) source is fitted against the rotated box, so a tier's
/// long edge bounds the source's long edge.
pub(crate) fn fit_size(width: u32, height: u32, maximum: (u32, u32)) -> (u32, u32) {
    let maximum = if (width < height) != (maximum.0 < maximum.1) {
        (maximum.1, maximum.0)
    } else {
        maximum
    };
    if width <= maximum.0 && height <= maximum.1 {
        return ((width & !1).max(2), (height & !1).max(2));
    }
    // Single-precision minimum scale, then 2*round(x/2).
    let scale = (maximum.0 as f32 / width as f32)
        .min(maximum.1 as f32 / height as f32)
        .min(1.0);
    (
        ((width as f32 * scale * 0.5).round() as u32 * 2).max(2),
        ((height as f32 * scale * 0.5).round() as u32 * 2).max(2),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portrait_sources_use_the_rotated_tier_box() {
        // A rotated 1080p panel stays 1080x1920 at the 1080p tier.
        assert_eq!(output_size(1080, 1920, Tier::P1080), (1080, 1920));
        assert_eq!(output_size(1920, 1080, Tier::P1080), (1920, 1080));
        // A rotated 4K panel at the 1080p tier keeps its orientation.
        assert_eq!(output_size(2160, 3840, Tier::P1080), (1080, 1920));
        // 16:10 at the 1440p tier is bounded by the short edge.
        assert_eq!(output_size(2560, 1600, Tier::P1440), (2304, 1440));
        assert_eq!(output_size(1600, 2560, Tier::P1440), (1440, 2304));
        assert_eq!(Tier::P720.down(), None);
        assert_eq!(Tier::P1440.up(), Some(Tier::P2160));
        assert_eq!(Tier::P2160.up(), None);
    }

    #[test]
    fn tier_rates_follow_size_frame_rate_and_colour() {
        // 超清 on a 2560x1600 screen is output at 2304x1440.
        assert_eq!(tier_rate(Tier::P1440, (2560, 1600), 60, false), 21_714_286);
        assert_eq!(tier_rate(Tier::P1440, (2560, 1600), 30, false), 10_857_143);
        assert_eq!(tier_rate(Tier::P1440, (2560, 1600), 60, true), 43_428_571);
        // Above 60 FPS the rate grows with the square root.
        assert_eq!(tier_rate(Tier::P1080, (1920, 1080), 144, false), 12_393_547);
    }
}
