use crate::i18n::Language;

/// Windows' display language: Chinese reads 简体中文, anything else English.
pub(crate) fn system_language() -> Language {
    // SAFETY: no arguments; returns the user's UI language ID.
    let id = unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() };
    const LANG_CHINESE: u16 = 0x04;
    if id & 0x3ff == LANG_CHINESE {
        Language::ZhCn
    } else {
        Language::En
    }
}
