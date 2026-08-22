//! Normalization: what "the same string" means to a fuzzy index.
//!
//! It reaches stored bytes — two records normalize to the grams they are found
//! by — so the profile is **declared** and folds into the `STRUCT_HASH`
//! (RFC 0056). Changing it yields a new type, like every other stored-byte
//! knob.
//!
//! ## The dependency this deliberately does not take
//!
//! This is a Latin diacritic fold, not a Unicode collation engine. It takes no
//! `unicode-normalization` dependency, because the same code compiles into a
//! wasm artifact where every kilobyte is argued for, and because the promise a
//! full collation makes is one this cannot keep anyway without tailoring per
//! locale.
//!
//! So the limits are stated rather than papered over: no Turkish dotless-i
//! rule, no Greek final sigma, no CJK segmentation. CJK still works — as
//! substring matching, since ideographs are their own grams. A stronger fold
//! is a later declared value, and being declared, it is a new type when it
//! changes.

/// How an indexed string is normalized before it is cut into grams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fold {
    /// Lowercase, collapse whitespace, fold Latin diacritics — so `José` and
    /// `Jose` share grams. The default.
    #[default]
    Latin,
    /// Lowercase and collapse whitespace only.
    None,
}

impl Fold {
    /// The declared spelling, as it folds into the `STRUCT_HASH`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Latin => "latin",
            Self::None => "none",
        }
    }
}

/// Normalize `input` to the codepoints a gram decomposition runs over.
///
/// Lowercasing happens **first**, so the fold table below only has to carry
/// lowercase forms — `À` arrives as `à`. Whitespace of any kind collapses to a
/// single ASCII space, and leading/trailing whitespace is dropped, so
/// `"  Ada   Lovelace "` and `"ada lovelace"` are the same string here.
#[must_use]
pub fn normalize(input: &str, fold: Fold) -> Vec<char> {
    let mut out = Vec::with_capacity(input.len());
    let mut gap = false;
    for ch in input.chars().flat_map(char::to_lowercase) {
        if ch.is_whitespace() {
            // Only a *separating* run becomes a space; a leading one is
            // dropped, and a trailing one never gets its space emitted.
            gap = !out.is_empty();
            continue;
        }
        if gap {
            out.push(' ');
            gap = false;
        }
        match fold {
            Fold::None => out.push(ch),
            Fold::Latin => push_folded(ch, &mut out),
        }
    }
    out
}

/// Push `ch`'s Latin fold — one char for most, several for the ligatures and
/// `ß`, and the char itself when nothing applies.
fn push_folded(ch: char, out: &mut Vec<char>) {
    match ch {
        // Latin-1 Supplement, lowercase half.
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => out.push('a'),
        'ç' => out.push('c'),
        'è' | 'é' | 'ê' | 'ë' => out.push('e'),
        'ì' | 'í' | 'î' | 'ï' => out.push('i'),
        'ð' => out.push('d'),
        'ñ' => out.push('n'),
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' => out.push('o'),
        'ù' | 'ú' | 'û' | 'ü' => out.push('u'),
        'ý' | 'ÿ' => out.push('y'),
        // Expansions: one codepoint in, two out. They are the reason this
        // takes `&mut Vec` rather than returning a `char`.
        'æ' => out.extend(['a', 'e']),
        'þ' => out.extend(['t', 'h']),
        // `ß` has no lowercase mapping of its own, so it arrives here intact.
        'ß' => out.extend(['s', 's']),
        _ => push_extended_a(ch, out),
    }
}

/// Latin Extended-A (U+0100…U+017F), lowercase forms.
#[allow(clippy::too_many_lines)]
fn push_extended_a(ch: char, out: &mut Vec<char>) {
    match ch {
        'ā' | 'ă' | 'ą' => out.push('a'),
        'ć' | 'ĉ' | 'ċ' | 'č' => out.push('c'),
        'ď' | 'đ' => out.push('d'),
        'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => out.push('e'),
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => out.push('g'),
        'ĥ' | 'ħ' => out.push('h'),
        'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => out.push('i'),
        'ĵ' => out.push('j'),
        'ķ' | 'ĸ' => out.push('k'),
        'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => out.push('l'),
        'ń' | 'ņ' | 'ň' | 'ŋ' | 'ŉ' => out.push('n'),
        'ō' | 'ŏ' | 'ő' => out.push('o'),
        'ŕ' | 'ŗ' | 'ř' => out.push('r'),
        'ś' | 'ŝ' | 'ş' | 'š' | 'ſ' => out.push('s'),
        'ţ' | 'ť' | 'ŧ' => out.push('t'),
        'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => out.push('u'),
        'ŵ' => out.push('w'),
        'ŷ' => out.push('y'),
        'ź' | 'ż' | 'ž' => out.push('z'),
        'ĳ' => out.extend(['i', 'j']),
        'œ' => out.extend(['o', 'e']),
        // Everything else — CJK, Cyrillic, digits, punctuation — passes
        // through as itself. Nothing is dropped: a character this table does
        // not know is still perfectly good gram material.
        other => out.push(other),
    }
}

#[cfg(test)]
mod tests {
    use super::{Fold, normalize};

    fn s(input: &str, fold: Fold) -> String {
        normalize(input, fold).into_iter().collect()
    }

    #[test]
    fn lowercases_folds_and_collapses() {
        assert_eq!(s("José", Fold::Latin), "jose");
        assert_eq!(s("  Ada   Lovelace ", Fold::Latin), "ada lovelace");
        assert_eq!(s("ÀÉÎÕÜ", Fold::Latin), "aeiou");
        assert_eq!(s("Łódź", Fold::Latin), "lodz");
    }

    #[test]
    fn expansions_yield_more_characters_than_they_consume() {
        // The case that forces the `&mut Vec` signature — and the reason a
        // gram count is derived from the *normalized* length, never the input's.
        assert_eq!(s("Straße", Fold::Latin), "strasse");
        assert_eq!(s("Æon", Fold::Latin), "aeon");
        assert_eq!(s("Œuvre", Fold::Latin), "oeuvre");
    }

    #[test]
    fn fold_none_keeps_the_diacritics() {
        assert_eq!(s("José", Fold::None), "josé");
        // …but still lowercases and collapses, which is not the diacritics'
        // business.
        assert_eq!(s(" JOSÉ  X ", Fold::None), "josé x");
    }

    #[test]
    fn unknown_scripts_pass_through_intact() {
        // Nothing is dropped: an unmapped character is still gram material,
        // which is what makes CJK work as substring matching.
        assert_eq!(s("東京", Fold::Latin), "東京");
        assert_eq!(s("Привет", Fold::Latin), "привет");
        assert_eq!(s("42!", Fold::Latin), "42!");
    }

    #[test]
    fn whitespace_only_input_normalizes_to_nothing() {
        assert_eq!(s("   \t\n ", Fold::Latin), "");
        assert_eq!(s("", Fold::Latin), "");
    }
}
