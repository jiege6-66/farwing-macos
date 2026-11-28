//! Local display geometry and refresh budget for the viewer.
use crate::media::LocalDisplayInfo;
use anyhow::{Context, Result, ensure};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayCopyDisplayMode, CGDisplayMode, CGError, CGGetActiveDisplayList,
    CGMainDisplayID,
};
use std::{ffi::c_void, ptr::null_mut};

type DisplayMode = (CGDirectDisplayID, (u32, u32), f64);

pub fn detect_local_display() -> Result<LocalDisplayInfo> {
    let main = CGMainDisplayID();
    let displays = active_displays()?
        .into_iter()
        .map(read_mode)
        .collect::<Result<Vec<_>>>()?;
    select_local_display(main, &displays)
}

fn select_local_display(
    main: CGDirectDisplayID,
    displays: &[DisplayMode],
) -> Result<LocalDisplayInfo> {
    let &(_, (width, height), _) = displays
        .iter()
        .find(|(id, ..)| *id == main)
        .context("no main local display was detected")?;
    ensure!(
        width != 0 && height != 0,
        "local display reported an invalid resolution"
    );
    // Main display geometry and the fastest active refresh serve different purposes.
    let refresh_hz = displays
        .iter()
        .map(|&(_, _, hz)| hz)
        .filter(|hz| valid_refresh(*hz))
        .map(|hz| hz.round() as u32)
        .fold(30, u32::max);
    Ok(LocalDisplayInfo {
        width,
        height,
        refresh_hz,
    })
}

fn active_displays() -> Result<Vec<CGDirectDisplayID>> {
    let mut count = 0;
    // SAFETY: a null array queries the count without writing display IDs.
    let status = unsafe { CGGetActiveDisplayList(0, null_mut(), &mut count) };
    ensure!(
        status == CGError::Success,
        "failed to count local displays: {status:?}"
    );
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut ids = vec![0; count as usize];
    let mut written = 0;
    // SAFETY: ids has room for count display IDs; written is a valid output pointer.
    let status = unsafe { CGGetActiveDisplayList(count, ids.as_mut_ptr(), &mut written) };
    ensure!(
        status == CGError::Success,
        "failed to enumerate local displays: {status:?}"
    );
    ids.truncate(written as usize);
    Ok(ids)
}

fn read_mode(id: CGDirectDisplayID) -> Result<DisplayMode> {
    let mode = CGDisplayCopyDisplayMode(id);
    let (size, rate) = if let Some(mode) = mode.as_deref() {
        (
            (
                u32::try_from(CGDisplayMode::pixel_width(Some(mode)))?,
                u32::try_from(CGDisplayMode::pixel_height(Some(mode)))?,
            ),
            CGDisplayMode::refresh_rate(Some(mode)),
        )
    } else {
        ((0, 0), 0.0)
    };
    let rate = if valid_refresh(rate) {
        rate
    } else {
        nominal_refresh(id)
    };
    Ok((id, size, rate))
}

fn valid_refresh(rate: f64) -> bool {
    rate.is_finite() && rate > 0.0
}

// The current Quartz mode can report 0 Hz (e.g. on a built-in Retina display).
// Query the nominal period without starting the display link or using AppKit.
fn nominal_refresh(id: CGDirectDisplayID) -> f64 {
    const CV_TIME_IS_INDEFINITE: i32 = 1;
    let mut link = null_mut();
    // SAFETY: the output pointer is valid. On success CoreVideo gives us an owned link.
    let status = unsafe { CVDisplayLinkCreateWithCGDisplay(id, &mut link) };
    if status != 0 || link.is_null() {
        return 0.0;
    }
    // SAFETY: link is live and owned; read the period and release it exactly once.
    let period = unsafe {
        let period = CVDisplayLinkGetNominalOutputVideoRefreshPeriod(link);
        CVDisplayLinkRelease(link);
        period
    };
    if period.flags & CV_TIME_IS_INDEFINITE == 0 && period.value > 0 && period.scale > 0 {
        f64::from(period.scale) / period.value as f64
    } else {
        0.0
    }
}

#[repr(C)]
struct CVTime {
    value: i64,
    scale: i32,
    flags: i32,
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVDisplayLinkCreateWithCGDisplay(display: CGDirectDisplayID, link: *mut *mut c_void) -> i32;
    fn CVDisplayLinkGetNominalOutputVideoRefreshPeriod(link: *mut c_void) -> CVTime;
    fn CVDisplayLinkRelease(link: *mut c_void);
}

#[cfg(test)]
mod tests {
    use super::select_local_display;

    #[test]
    fn uses_main_display_geometry_and_fastest_active_refresh() {
        let displays = [(2, (3840, 2160), 143.6), (1, (2560, 1664), 60.0)];
        let display = select_local_display(1, &displays).unwrap();
        assert_eq!((display.width, display.height), (2560, 1664));
        assert_eq!(display.refresh_hz, 144);
    }

    #[test]
    fn ignores_invalid_rates_and_applies_a_30_hz_floor() {
        let displays = [
            (1, (2560, 1664), 24.0),
            (2, (1920, 1080), f64::NAN),
            (3, (1920, 1080), f64::INFINITY),
            (4, (1920, 1080), 0.0),
        ];
        assert_eq!(select_local_display(1, &displays).unwrap().refresh_hz, 30);
        assert_eq!(
            select_local_display(4, &displays[1..]).unwrap().refresh_hz,
            30
        );
    }

    #[test]
    fn rejects_missing_main_display() {
        assert!(select_local_display(1, &[]).is_err());
        assert!(select_local_display(1, &[(2, (3840, 2160), 60.0)]).is_err());
    }

    #[test]
    fn rejects_zero_main_display_dimensions() {
        for size in [(0, 1664), (2560, 0)] {
            assert!(select_local_display(1, &[(1, size, 60.0)]).is_err());
        }
    }
}
