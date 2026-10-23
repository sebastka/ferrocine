//! Language codes: providers and files use ISO 639-1 (`fr`), 639-2/T
//! (`fra`), 639-2/B (`fre`) or locales (`fr-FR`). Everything is normalised
//! to ISO 639-1 for comparison.

/// (639-1, 639-2/T, 639-2/B when it differs, English name).
const LANGUAGES: [(&str, &str, &str, &str); 46] = [
    ("ar", "ara", "", "Arabic"),
    ("bg", "bul", "", "Bulgarian"),
    ("bn", "ben", "", "Bengali"),
    ("ca", "cat", "", "Catalan"),
    ("cs", "ces", "cze", "Czech"),
    ("da", "dan", "", "Danish"),
    ("de", "deu", "ger", "German"),
    ("el", "ell", "gre", "Greek"),
    ("en", "eng", "", "English"),
    ("es", "spa", "", "Spanish"),
    ("et", "est", "", "Estonian"),
    ("eu", "eus", "baq", "Basque"),
    ("fa", "fas", "per", "Persian"),
    ("fi", "fin", "", "Finnish"),
    ("fr", "fra", "fre", "French"),
    ("he", "heb", "", "Hebrew"),
    ("hi", "hin", "", "Hindi"),
    ("hr", "hrv", "", "Croatian"),
    ("hu", "hun", "", "Hungarian"),
    ("id", "ind", "", "Indonesian"),
    ("is", "isl", "ice", "Icelandic"),
    ("it", "ita", "", "Italian"),
    ("ja", "jpn", "", "Japanese"),
    ("ko", "kor", "", "Korean"),
    ("lt", "lit", "", "Lithuanian"),
    ("lv", "lav", "", "Latvian"),
    ("ms", "msa", "may", "Malay"),
    ("nb", "nob", "", "Norwegian Bokmål"),
    ("nl", "nld", "dut", "Dutch"),
    ("nn", "nno", "", "Norwegian Nynorsk"),
    ("no", "nor", "", "Norwegian"),
    ("pl", "pol", "", "Polish"),
    ("pt", "por", "", "Portuguese"),
    ("ro", "ron", "rum", "Romanian"),
    ("ru", "rus", "", "Russian"),
    ("sk", "slk", "slo", "Slovak"),
    ("sl", "slv", "", "Slovenian"),
    ("sr", "srp", "", "Serbian"),
    ("sv", "swe", "", "Swedish"),
    ("ta", "tam", "", "Tamil"),
    ("th", "tha", "", "Thai"),
    ("tr", "tur", "", "Turkish"),
    ("uk", "ukr", "", "Ukrainian"),
    ("vi", "vie", "", "Vietnamese"),
    ("zh", "zho", "chi", "Chinese"),
    ("gl", "glg", "", "Galician"),
];

fn find(code: &str) -> Option<&'static (&'static str, &'static str, &'static str, &'static str)> {
    // `fr-FR`, `pt_BR`, `en.forced`: the language part.
    let code = code
        .trim()
        .split(['-', '_', '.'])
        .next()
        .unwrap_or_default()
        .to_lowercase();
    LANGUAGES
        .iter()
        .find(|(a, t, b, _)| *a == code || *t == code || (!b.is_empty() && *b == code))
}

/// ISO 639-1 code of any known form, e.g. `fre` → `fr`.
pub fn iso1(code: &str) -> Option<&'static str> {
    find(code).map(|l| l.0)
}

/// ISO 639-2/T code (TheTVDB's), e.g. `fr-FR` → `fra`.
pub fn iso2(code: &str) -> Option<&'static str> {
    find(code).map(|l| l.1)
}

/// English name of a language.
pub fn name(code: &str) -> Option<&'static str> {
    find(code).map(|l| l.3)
}

/// Whether two codes name the same language.
pub fn same(a: &str, b: &str) -> bool {
    matches!((iso1(a), iso1(b)), (Some(x), Some(y)) if x == y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_every_form() {
        for code in ["fr", "fra", "fre", "fr-FR", "FR", "fr_CA"] {
            assert_eq!(iso1(code), Some("fr"), "{code}");
        }
        assert_eq!(iso2("de-DE"), Some("deu"));
        assert_eq!(iso2("ger"), Some("deu"));
        assert!(same("chi", "zh-TW"));
        assert!(!same("en", "und"));
        assert_eq!(iso1("xx"), None);
        assert_eq!(name("ja"), Some("Japanese"));
    }
}
