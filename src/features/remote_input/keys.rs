//! HID usages for the shared input stream, independent of native key codes.

pub(crate) const KEYBOARD: u32 = 0x07 << 16;
pub(crate) const CONSUMER: u32 = 0x0c << 16;

/// Whether the Windows host has an injection mapping for this usage. This is
/// the usage set (including aliases) of the Windows scan table, not all of HID.
pub(crate) fn supported(usage: u32) -> bool {
    let id = usage & 0xffff;
    match usage & 0xffff_0000 {
        KEYBOARD => {
            matches!(id, 0x04..=0x65 | 0x67..=0x73 | 0x85 | 0x87..=0x8b | 0x90..=0x91 | 0xe0..=0xe7)
        }
        CONSUMER => {
            matches!(id, 0xb5..=0xb7 | 0xcd | 0xe2 | 0xe9..=0xea | 0x183 | 0x18a | 0x192 | 0x194 | 0x221 | 0x223..=0x227 | 0x22a)
        }
        _ => false,
    }
}

/// Caps Lock, Num Lock and Scroll Lock, in `LockState` field order.
pub(crate) const LOCKS: [u32; 3] = [KEYBOARD | 0x39, KEYBOARD | 0x53, KEYBOARD | 0x47];

pub(crate) fn lock_index(usage: u32) -> Option<usize> {
    LOCKS.iter().position(|lock| *lock == usage)
}

/// Shift, Ctrl, Alt and GUI keys on either side.
pub(crate) fn modifier(usage: u32) -> bool {
    (KEYBOARD | 0xe0..=KEYBOARD | 0xe7).contains(&usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    mod legacy {
        const K: u32 = 0x07 << 16;
        pub(super) const LOCKS: [u32; 3] = [K | 0x39, K | 0x53, K | 0x47];

        pub(super) fn lock_index(usage: u32) -> Option<usize> {
            LOCKS.iter().position(|lock| *lock == usage)
        }

        pub(super) fn modifier(usage: u32) -> bool {
            (K | 0xe0..=K | 0xe7).contains(&usage)
        }
    }

    #[test]
    fn locks_and_modifiers_match_legacy_windows_classification() {
        assert_eq!(LOCKS, legacy::LOCKS);
        for page in [0, KEYBOARD, CONSUMER, 0xffff_0000] {
            for id in 0..=0xffff {
                let usage = page | id;
                assert_eq!(lock_index(usage), legacy::lock_index(usage), "{usage:#x}");
                assert_eq!(modifier(usage), legacy::modifier(usage), "{usage:#x}");
            }
        }
    }

    #[test]
    fn supported_usages_include_aliases_but_not_unmapped_keys() {
        for usage in [
            KEYBOARD | 0x04,
            KEYBOARD | 0x32,
            KEYBOARD | 0x90,
            CONSUMER | 0x22a,
        ] {
            assert!(supported(usage), "{usage:#x}");
        }
        for usage in [
            0x04,
            KEYBOARD | 0x03,
            KEYBOARD | 0x66,
            CONSUMER | 0xe0,
            0x0107_0004,
        ] {
            assert!(!supported(usage), "{usage:#x}");
        }
    }
}
