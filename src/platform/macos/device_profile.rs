//! Native macOS device facts for registration and the direct peer name.
use crate::{account::hardware::Hardware, i18n::t};
use anyhow::{Context, Result, ensure};
use objc2::rc::autoreleasepool;
use objc2_core_foundation::{CFDictionary, CFRetained, CFString, CFType};
use objc2_core_graphics::{CGDisplayCopyDisplayMode, CGDisplayMode, CGMainDisplayID};
use objc2_foundation::NSProcessInfo;
use std::{
    ffi::CStr,
    io,
    mem::offset_of,
    ptr::{NonNull, null_mut},
    slice,
};
use system_configuration_sys::{
    dynamic_store::SCDynamicStoreCopyValue,
    dynamic_store_copy_specific::SCDynamicStoreCopyComputerName,
};
use uuid::Uuid;

/// The computer name shown in System Settings, not the DNS hostname.
pub(crate) fn computer_name() -> Result<String> {
    // SAFETY: a null store requests a temporary session; the encoding is optional.
    let raw = unsafe { SCDynamicStoreCopyComputerName(null_mut(), null_mut()) };
    let raw = NonNull::new(raw.cast_mut().cast::<CFString>())
        .context(t().system.profile_identity_missing)?;
    // SAFETY: Copy returned an owned CFString; CFRetained releases it.
    let name = unsafe { CFRetained::from_raw(raw) }.to_string();
    ensure!(
        !name.trim().is_empty(),
        "{}",
        t().system.profile_identity_missing
    );
    Ok(name)
}

impl Hardware {
    /// Read the local profile. The name is required as on Windows; unavailable
    /// optional fields are recorded without blocking registration. Unlike on
    /// Windows, the UUID is optional: `gethostuuid` can fail or return a sentinel.
    pub(crate) fn read() -> Result<Self> {
        let labels = &t().system;
        let mut value = Self {
            name: computer_name()?,
            // machine_guid is a Windows registry value. GPU names are not
            // collected here; CoreGraphics supplies display dimensions only.
            ..Self::default()
        };
        let mut text = |label, result| field(&mut value.errors, label, result);
        value.system_uuid = text(t().settings.system_uuid, system_uuid());
        value.base_board = text(labels.profile_base_board, sysctl_string(c"hw.model"));
        value.cpu = text(
            labels.profile_cpu,
            sysctl_string(c"machdep.cpu.brand_string"),
        );
        // NSProcessInfo always supplies a version; only the build lookup can fail.
        value.os = os_version();
        let build = text(labels.profile_os, sysctl_string(c"kern.osversion"));
        if !build.is_empty() {
            value.os.push_str(&format!(" ({build})"));
        }
        value.screen = text(t().viewer.resolution, Ok(screen_size()));
        value.mac = text(labels.profile_mac, primary_mac());
        value.memory = field(&mut value.errors, labels.profile_memory, memory_mib());
        Ok(value)
    }
}

/// Optional text and memory fields use their default value when unavailable.
fn field<T: Default + PartialEq>(errors: &mut Vec<String>, label: &str, result: Result<T>) -> T {
    match result {
        Ok(value) if value != T::default() => value,
        Ok(_) => {
            errors.push((t().system.profile_not_provided)(label));
            T::default()
        }
        Err(error) => {
            errors.push((t().system.profile_field_failed)(
                label,
                &format!("{error:#}"),
            ));
            T::default()
        }
    }
}

/// With no buffer, ask the kernel how much space the named value needs.
fn sysctl(name: &CStr, output: Option<&mut [u8]>) -> Result<usize> {
    let (pointer, mut len) =
        output.map_or((null_mut(), 0), |buf| (buf.as_mut_ptr().cast(), buf.len()));
    // SAFETY: the name is NUL-terminated, the optional buffer holds len bytes,
    // and the final two arguments make this a read-only query.
    let status = unsafe { libc::sysctlbyname(name.as_ptr(), pointer, &mut len, null_mut(), 0) };
    if status != 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| name.to_string_lossy().into_owned());
    }
    Ok(len)
}

fn sysctl_string(name: &CStr) -> Result<String> {
    let mut bytes = vec![0; sysctl(name, None)?];
    let len = sysctl(name, Some(&mut bytes))?;
    bytes.truncate(len);
    Ok(CStr::from_bytes_with_nul(&bytes)?
        .to_str()?
        .trim()
        .to_owned())
}

fn memory_mib() -> Result<i64> {
    let mut bytes = [0; size_of::<u64>()];
    let len = sysctl(c"hw.memsize", Some(&mut bytes))?;
    ensure!(len == bytes.len(), "unexpected hw.memsize size: {len}");
    Ok((u64::from_ne_bytes(bytes) / (1024 * 1024)) as i64)
}

fn os_version() -> String {
    let version = autoreleasepool(|_| NSProcessInfo::processInfo().operatingSystemVersion());
    let mut os = format!("macOS {}.{}", version.majorVersion, version.minorVersion);
    if version.patchVersion != 0 {
        os.push_str(&format!(".{}", version.patchVersion));
    }
    os
}

fn screen_size() -> String {
    let Some(mode) = CGDisplayCopyDisplayMode(CGMainDisplayID()) else {
        return String::new();
    };
    // Report the mode's native pixels on Retina. Windows deliberately keeps
    // reporting logical dimensions via SM_CXSCREEN and SM_CYSCREEN.
    let width = CGDisplayMode::pixel_width(Some(&mode));
    let height = CGDisplayMode::pixel_height(Some(&mode));
    if width == 0 || height == 0 {
        return String::new();
    }
    format!("{width}x{height}")
}

fn system_uuid() -> Result<String> {
    let mut bytes = [0; 16];
    let timeout = libc::timespec {
        tv_sec: 1,
        tv_nsec: 0,
    };
    // SAFETY: gethostuuid writes a 16-byte UUID and reads the timeout.
    if unsafe { libc::gethostuuid(bytes.as_mut_ptr(), &timeout) } != 0 {
        return Err(io::Error::last_os_error()).context("gethostuuid");
    }
    if bytes == [0; 16] || bytes == [255; 16] {
        return Ok(String::new());
    }
    Ok(Uuid::from_bytes(bytes).to_string().to_uppercase())
}

/// The interface macOS selected in its global IPv4 or IPv6 state.
fn primary_interface(state_key: &str) -> Option<String> {
    let key = CFString::from_str(state_key);
    // SAFETY: the store can be null and key is a live CFString.
    let raw = unsafe { SCDynamicStoreCopyValue(null_mut(), (&*key as *const CFString).cast()) };
    let raw = NonNull::new(raw.cast_mut().cast::<CFType>())?;
    // SAFETY: Copy returned an owned property list value.
    let state = unsafe { CFRetained::from_raw(raw) };
    let dictionary = state.downcast_ref::<CFDictionary>()?;
    // SAFETY: property list dictionaries have CFString keys and CF object values.
    let dictionary = unsafe { dictionary.cast_unchecked::<CFString, CFType>() };
    let interface = dictionary.get(&CFString::from_str("PrimaryInterface"))?;
    Some(interface.downcast_ref::<CFString>()?.to_string())
}

fn primary_mac() -> Result<String> {
    let mut list = null_mut();
    // SAFETY: getifaddrs writes the head pointer into list on success.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return Err(io::Error::last_os_error()).context("getifaddrs");
    }
    let addresses = InterfaceAddresses(list);
    let mac = ["State:/Network/Global/IPv4", "State:/Network/Global/IPv6"]
        .into_iter()
        .filter_map(primary_interface)
        .find_map(|interface| addresses.mac(&interface))
        // For example, a VPN default route may have no hardware address.
        .or_else(|| addresses.mac("en0"));
    Ok(mac
        .map(|mac| mac.map(|byte| format!("{byte:02X}")).join(":"))
        .unwrap_or_default())
}

struct InterfaceAddresses(*mut libc::ifaddrs);

impl InterfaceAddresses {
    fn mac(&self, interface: &str) -> Option<[u8; 6]> {
        let mut next = self.0;
        while !next.is_null() {
            // SAFETY: getifaddrs owns this list and its pointers until drop.
            let entry = unsafe { &*next };
            next = entry.ifa_next;
            let addr = entry.ifa_addr;
            if addr.is_null() || entry.ifa_name.is_null() {
                continue;
            }
            // SAFETY: the non-null pointers refer to a sockaddr header and a C string.
            if unsafe {
                (*addr).sa_family as i32 != libc::AF_LINK
                    || CStr::from_ptr(entry.ifa_name).to_bytes() != interface.as_bytes()
            } {
                continue;
            }
            // SAFETY: the AF_LINK record holds sa_len bytes; its payload may
            // differ from the fixed-size sockaddr_dl declaration in libc.
            let bytes = unsafe { slice::from_raw_parts(addr.cast(), usize::from((*addr).sa_len)) };
            if let Some(mac) = link_mac(bytes) {
                return Some(mac);
            }
        }
        None
    }
}

impl Drop for InterfaceAddresses {
    fn drop(&mut self) {
        // SAFETY: this is the list returned by getifaddrs, freed only here.
        unsafe { libc::freeifaddrs(self.0) };
    }
}

/// In sockaddr_dl, the link address follows the variable-length interface name.
fn link_mac(bytes: &[u8]) -> Option<[u8; 6]> {
    let name_len = usize::from(*bytes.get(offset_of!(libc::sockaddr_dl, sdl_nlen))?);
    let mac_len = usize::from(*bytes.get(offset_of!(libc::sockaddr_dl, sdl_alen))?);
    let start = offset_of!(libc::sockaddr_dl, sdl_data) + name_len;
    bytes.get(start..start + mac_len)?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_mac_follows_the_interface_name() {
        let mut bytes = vec![0; offset_of!(libc::sockaddr_dl, sdl_data)];
        bytes[offset_of!(libc::sockaddr_dl, sdl_nlen)] = 7;
        bytes[offset_of!(libc::sockaddr_dl, sdl_alen)] = 6;
        bytes.extend(b"vlan100");
        bytes.extend([0, 1, 2, 3, 4, 255]);
        assert_eq!(link_mac(&bytes), Some([0, 1, 2, 3, 4, 255]));
        assert_eq!(link_mac(&bytes[..bytes.len() - 1]), None);
        bytes[offset_of!(libc::sockaddr_dl, sdl_alen)] = 0;
        assert_eq!(link_mac(&bytes), None);
    }
}
