use objc2::rc::autoreleasepool;
use objc2_foundation::NSLocale;

pub(crate) fn preferred_language() -> Option<String> {
    autoreleasepool(|_| {
        NSLocale::preferredLanguages()
            .firstObject()
            .map(|tag| tag.to_string())
    })
}
