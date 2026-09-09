//! Fonts shipped inside the MCP binary.
//!
//! The TTF bytes are included at compile time so a release executable does
//! not depend on its current working directory or on a separate font
//! download. The MCP copies these bytes into the managed job before an
//! editable render, where the normal project-path checks still apply.

use rustybuzz::Face;
use sha2::{Digest, Sha256};

/// The intended use of a bundled face. The public typesetting request keeps
/// accepting an explicit font path, so callers that need emphasis can select
/// COMIC_NEUE_BOLD while ordinary dialogue uses COMIC_NEUE_REGULAR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundledFontRole {
    /// Ordinary speech and dialogue text.
    Dialogue,
    /// Explicit emphasis or a heavier title treatment.
    Emphasis,
    /// Handwritten text and glyph coverage fallback, including Vietnamese.
    HandwritingAndVietnameseFallback,
    /// Compact symbol coverage used only for graphemes absent from the
    /// requested primary and earlier fallback faces.
    SymbolFallback,
}

/// Metadata and bytes for one embedded TTF.
#[derive(Debug, Clone, Copy)]
pub struct BundledFont {
    pub id: &'static str,
    pub file_name: &'static str,
    pub role: BundledFontRole,
    pub bytes: &'static [u8],
    pub sha256: &'static str,
}

impl BundledFont {
    /// Return the lower-case SHA-256 digest of the embedded bytes.
    pub fn sha256_hex(self) -> String {
        let digest = Sha256::digest(self.bytes);
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// Verify the compile-time asset against the recorded upstream download
    /// hash before it is written into a job.
    pub fn has_expected_sha256(self) -> bool {
        self.sha256_hex() == self.sha256
    }

    /// Return whether this face maps every non-control character in text.
    pub fn covers(self, text: &str) -> bool {
        let Some(face) = Face::from_slice(self.bytes, 0) else {
            return false;
        };
        text.chars()
            .filter(|character| !character.is_control())
            .all(|character| {
                face.glyph_index(character)
                    .is_some_and(|glyph| glyph.0 != 0)
            })
    }
}

/// Comic Neue Regular is the default dialogue face.
pub static COMIC_NEUE_REGULAR: BundledFont = BundledFont {
    id: "comic-neue-regular",
    file_name: "ComicNeue-Regular.ttf",
    role: BundledFontRole::Dialogue,
    bytes: include_bytes!("../assets/fonts/ComicNeue-Regular.ttf"),
    sha256: "a0ee5a37c8b27c4db0700137d928598b1e23b0089e1546a8961909176b779360",
};

/// Comic Neue Bold is available for explicit emphasis.
pub static COMIC_NEUE_BOLD: BundledFont = BundledFont {
    id: "comic-neue-bold",
    file_name: "ComicNeue-Bold.ttf",
    role: BundledFontRole::Emphasis,
    bytes: include_bytes!("../assets/fonts/ComicNeue-Bold.ttf"),
    sha256: "3e7e5fccfd7e0788f317b43312151c1bd5cf058c9697a8d83eac3939050bd61e",
};

/// Patrick Hand Regular contains the Vietnamese subset and is the bundled
/// grapheme fallback when Comic Neue lacks a required glyph.
pub static PATRICK_HAND_REGULAR: BundledFont = BundledFont {
    id: "patrick-hand-regular",
    file_name: "PatrickHand-Regular.ttf",
    role: BundledFontRole::HandwritingAndVietnameseFallback,
    bytes: include_bytes!("../assets/fonts/PatrickHand-Regular.ttf"),
    sha256: "0f173b3e6cb6d1af25babf7f0057c5ac4ee11f9992b0469bb817e967ef4ad0fc",
};

/// Noto Sans Symbols 2 supplies cross-platform symbols such as U+2764. It
/// is ordered after Patrick Hand so ordinary Vietnamese dialogue keeps its
/// intended face and only unsupported graphemes use this compact fallback.
pub static NOTO_SANS_SYMBOLS2_REGULAR: BundledFont = BundledFont {
    id: "noto-sans-symbols2-regular",
    file_name: "NotoSansSymbols2-Regular.ttf",
    role: BundledFontRole::SymbolFallback,
    bytes: include_bytes!("../assets/fonts/NotoSansSymbols2-Regular.ttf"),
    sha256: "41bf5d61b91184df45013e616b34e963a31036e04eed4aad673cc713a5e59133",
};

/// All faces shipped with the release, in their default preference order.
pub fn bundled_fonts() -> [&'static BundledFont; 4] {
    [
        &COMIC_NEUE_REGULAR,
        &COMIC_NEUE_BOLD,
        &PATRICK_HAND_REGULAR,
        &NOTO_SANS_SYMBOLS2_REGULAR,
    ]
}

/// Per-grapheme fallbacks appended after caller-supplied fallbacks. Patrick
/// Hand comes first because its official Vietnamese subset covers the common
/// precomposed and combining forms that Comic Neue's Latin subset does not;
/// Noto Sans Symbols 2 is reached only for symbol graphemes such as U+2764.
pub fn bundled_fallbacks() -> [&'static BundledFont; 3] {
    [
        &PATRICK_HAND_REGULAR,
        &NOTO_SANS_SYMBOLS2_REGULAR,
        &COMIC_NEUE_BOLD,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIETNAMESE_GLYPHS: &str = concat!(
        "ăâđêôơưĂÂĐÊÔƠƯ",
        "áàảãạắằẳẵặấầẩẫậ",
        "éèẻẽẹếềểễệ",
        "íìỉĩị",
        "óòỏõọốồổỗộớờởỡợ",
        "úùủũụứừửữự",
        "ýỳỷỹỵ",
    );

    #[test]
    fn embedded_faces_are_valid_and_hash_pinned() {
        for font in bundled_fonts() {
            assert!(
                font.has_expected_sha256(),
                "{} hash changed: expected {}, got {}",
                font.id,
                font.sha256,
                font.sha256_hex()
            );
            assert!(
                Face::from_slice(font.bytes, 0).is_some(),
                "{} is not a readable TTF",
                font.id
            );
        }
    }

    #[test]
    fn patrick_hand_covers_vietnamese_precomposed_and_combining_forms() {
        assert!(PATRICK_HAND_REGULAR.covers(VIETNAMESE_GLYPHS));
        assert!(PATRICK_HAND_REGULAR.covers("a\u{0301} e\u{0309} o\u{0323} u\u{0303}"));
        assert!(!COMIC_NEUE_REGULAR.covers("ơ ư ắ ệ"));
    }

    #[test]
    fn comic_neue_faces_cover_ascii_dialogue_and_have_distinct_roles() {
        assert!(COMIC_NEUE_REGULAR.covers("Hello, world!"));
        assert!(COMIC_NEUE_BOLD.covers("EMPHASIS"));
        assert_eq!(COMIC_NEUE_REGULAR.role, BundledFontRole::Dialogue);
        assert_eq!(COMIC_NEUE_BOLD.role, BundledFontRole::Emphasis);
        assert_eq!(
            PATRICK_HAND_REGULAR.role,
            BundledFontRole::HandwritingAndVietnameseFallback
        );
        assert_eq!(
            NOTO_SANS_SYMBOLS2_REGULAR.role,
            BundledFontRole::SymbolFallback
        );
        assert!(NOTO_SANS_SYMBOLS2_REGULAR.covers("❤"));
        assert!(!COMIC_NEUE_REGULAR.covers("❤"));
    }
}
