//! Global interner for high-cardinality-but-small short strings.
//!
//! Used for values with ~10-20 unique strings across 100k+ allocations per sync cycle:
//! iCloud UTI asset types (public.jpeg, public.heic, …) and provider source names.

use std::sync::{Arc, OnceLock};

use rustc_hash::FxHashMap;

static INTERNED: OnceLock<FxHashMap<&'static str, Arc<str>>> = OnceLock::new();

fn table() -> &'static FxHashMap<&'static str, Arc<str>> {
    INTERNED.get_or_init(|| {
        let mut m = FxHashMap::default();
        for &s in KNOWN_STRINGS {
            m.insert(s, Arc::from(s));
        }
        m
    })
}

/// Intern a string. Returns a shared Arc for known values; allocates a fresh Arc for unknowns.
pub(crate) fn intern(s: &str) -> Arc<str> {
    if let Some(arc) = table().get(s) {
        Arc::clone(arc)
    } else {
        Arc::from(s)
    }
}

/// All known strings that benefit from interning.
static KNOWN_STRINGS: &[&str] = &[
    // iCloud UTI asset types
    "public.jpeg",
    "public.heic",
    "public.heif",
    "public.png",
    "public.tiff",
    "org.webmproject.webp",
    "com.apple.quicktime-movie",
    "com.adobe.raw-image",
    "com.canon.cr2-raw-image",
    "com.canon.crw-raw-image",
    "com.sony.arw-raw-image",
    "com.fuji.raw-image",
    "com.panasonic.rw2-raw-image",
    "com.nikon.nrw-raw-image",
    "com.pentax.raw-image",
    "com.nikon.raw-image",
    "com.olympus.raw-image",
    "com.canon.cr3-raw-image",
    "com.olympus.or-raw-image",
    // Provider source names
    "icloud",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_string_returns_shared_arc() {
        let a = intern("public.jpeg");
        let b = intern("public.jpeg");
        // Same underlying pointer — no second allocation.
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn unknown_string_allocates_fresh_arc() {
        let a = intern("com.some.unknown-type");
        let b = intern("com.some.unknown-type");
        // Different Arcs — we don't cache unknowns.
        assert_eq!(a.as_ref(), b.as_ref());
    }
}
