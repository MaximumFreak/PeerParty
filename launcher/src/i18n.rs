//! The words the launcher window and the overlay show, in the user's language.
//!
//! Each language is one file in `launcher/locales/` (`key = text`, one per line, `#`
//! comments, `\n` for a line break, `{name}` for a value filled in), built into the
//! program. Every language has every key (a test checks); one missing would show in
//! English. A key with plural forms has `key.one`, `key.few`, `key.many` and `key.other`
//! variants (the ones its language uses), chosen by the value named `n`.
//!
//! The language is the system's display language unless the player picks one
//! (`Prefs.language`, or `PEERPARTY_LANG`). Logs, the command line and messages to other
//! players stay in English.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

/// The languages there are: code, the language's own name, its file.
pub const LANGS: &[(&str, &str, &str)] = &[
    ("en", "English", include_str!("../locales/en.txt")),
    ("de", "Deutsch", include_str!("../locales/de.txt")),
    ("es", "Español", include_str!("../locales/es.txt")),
    ("fr", "Français", include_str!("../locales/fr.txt")),
    ("it", "Italiano", include_str!("../locales/it.txt")),
    ("nl", "Nederlands", include_str!("../locales/nl.txt")),
    ("pl", "Polski", include_str!("../locales/pl.txt")),
    ("pt-BR", "Português (Brasil)", include_str!("../locales/pt-BR.txt")),
    ("ru", "Русский", include_str!("../locales/ru.txt")),
    ("uk", "Українська", include_str!("../locales/uk.txt")),
    ("tr", "Türkçe", include_str!("../locales/tr.txt")),
    ("ja", "日本語", include_str!("../locales/ja.txt")),
    ("ko", "한국어", include_str!("../locales/ko.txt")),
    ("zh-CN", "简体中文", include_str!("../locales/zh-CN.txt")),
    ("zh-TW", "繁體中文", include_str!("../locales/zh-TW.txt")),
];

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static TABLES: [OnceLock<HashMap<&'static str, &'static str>>; LANGS.len()] = [const { OnceLock::new() }; LANGS.len()];

/// Reads a language file: `key = text` lines.
pub fn parse(text: &'static str) -> HashMap<&'static str, &'static str> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.trim_start_matches('\u{feff}');
        if line.trim_start().starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        if k.is_empty() {
            continue;
        }
        // Only lines with escapes are copied (once per language, for the program's life).
        let v: &'static str = if v.contains('\\') { Box::leak(v.replace("\\n", "\n").replace("\\t", "\t").into_boxed_str()) } else { v };
        out.insert(k, v);
    }
    out
}

fn table(i: usize) -> &'static HashMap<&'static str, &'static str> {
    TABLES[i].get_or_init(|| parse(LANGS[i].2))
}

/// The language for a system language tag ("de-AT" -> "de", "zh-Hant-HK" -> "zh-TW",
/// "pt-PT" -> "pt-BR"), if there's one.
pub fn closest(tag: &str) -> Option<usize> {
    let tag = tag.trim().replace('_', "-");
    let lower = tag.to_ascii_lowercase();
    if let Some(i) = LANGS.iter().position(|l| l.0.eq_ignore_ascii_case(&tag)) {
        return Some(i);
    }
    let primary = lower.split('-').next().unwrap_or_default();
    let code = match primary {
        "zh" if ["hant", "tw", "hk", "mo"].iter().any(|t| lower.split('-').any(|p| p == *t)) => "zh-TW",
        "zh" => "zh-CN",
        "pt" => "pt-BR",
        // Ukrainian speakers who set Russian get Russian; nothing else stands in.
        other => other,
    };
    LANGS.iter().position(|l| l.0 == code)
}

/// Sets the language: a code from `LANGS`, or "" (or "auto") for the system's.
/// `PEERPARTY_LANG` wins over both.
pub fn set(choice: &str) {
    let env = std::env::var("PEERPARTY_LANG").ok().filter(|v| !v.trim().is_empty());
    let pick = env.as_deref().unwrap_or(choice);
    let i = if pick.is_empty() || pick.eq_ignore_ascii_case("auto") { None } else { closest(pick) };
    let i = i.or_else(|| crate::platform::languages().iter().find_map(|t| closest(t))).unwrap_or(0);
    CURRENT.store(i, Ordering::Relaxed);
}

/// The language in use, as a code from `LANGS`.
pub fn code() -> &'static str {
    LANGS[CURRENT.load(Ordering::Relaxed)].0
}

/// Which CJK font a fallback should prefer for the language in use (`platform::fallback_font`).
pub fn font_lang() -> &'static str {
    match code() {
        "ja" => "ja",
        "ko" => "ko",
        "zh-TW" => "zh-TW",
        _ => "zh-CN",
    }
}

/// A key's text in the language in use, else English, else the key itself.
pub fn tr(key: &'static str) -> &'static str {
    let i = CURRENT.load(Ordering::Relaxed);
    table(i).get(key).or_else(|| table(0).get(key)).copied().unwrap_or(key)
}

/// The plural form a language uses for `n` (CLDR's rules for whole numbers).
pub fn plural(lang: &str, n: i64) -> &'static str {
    let n = n.unsigned_abs();
    let (m10, m100) = (n % 10, n % 100);
    match lang {
        "ja" | "ko" | "zh-CN" | "zh-TW" => "other",
        "fr" | "pt-BR" => if n <= 1 { "one" } else { "other" },
        "ru" | "uk" => {
            if m10 == 1 && m100 != 11 {
                "one"
            } else if (2..=4).contains(&m10) && !(12..=14).contains(&m100) {
                "few"
            } else {
                "many"
            }
        }
        "pl" => {
            if n == 1 {
                "one"
            } else if (2..=4).contains(&m10) && !(12..=14).contains(&m100) {
                "few"
            } else {
                "many"
            }
        }
        _ => if n == 1 { "one" } else { "other" },
    }
}

fn lookup(i: usize, key: &str) -> Option<&'static str> {
    table(i).get(key).copied()
}

/// A key's text with its `{name}` values filled in. With a whole-number `n`, the key's
/// plural form for it.
pub fn trf(key: &'static str, args: &[(&str, String)]) -> String {
    let i = CURRENT.load(Ordering::Relaxed);
    let n = args.iter().find(|(k, _)| *k == "n").and_then(|(_, v)| v.parse::<i64>().ok());
    let pick = |i: usize| -> Option<&'static str> {
        match n {
            Some(n) => {
                let form = plural(LANGS[i].0, n);
                lookup(i, &format!("{key}.{form}"))
                    .or_else(|| lookup(i, &format!("{key}.other")))
                    .or_else(|| lookup(i, &format!("{key}.many")))
                    .or_else(|| lookup(i, key))
            }
            None => lookup(i, key),
        }
    };
    let mut s = pick(i).or_else(|| pick(0)).unwrap_or(key).to_string();
    for (k, v) in args {
        s = s.replace(&format!("{{{k}}}"), v);
    }
    s
}

/// `t!("key")` is the key's text; `t!("key", name = value, n = count)` fills it in.
#[macro_export]
macro_rules! t {
    ($key:literal) => {
        $crate::i18n::tr($key)
    };
    ($key:literal, $($name:ident = $val:expr),+ $(,)?) => {
        $crate::i18n::trf($key, &[$((stringify!($name), ($val).to_string())),+])
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::Path;

    fn base(k: &str) -> &str {
        for f in [".one", ".few", ".many", ".other"] {
            if let Some(b) = k.strip_suffix(f) {
                return b;
            }
        }
        k
    }

    fn holes(s: &str) -> HashSet<String> {
        let mut out = HashSet::new();
        let mut rest = s;
        while let Some(a) = rest.find('{') {
            let Some(b) = rest[a..].find('}') else { break };
            out.insert(rest[a + 1..a + b].to_string());
            rest = &rest[a + b..];
        }
        out
    }

    fn sources(dir: &Path, out: &mut String) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                sources(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push_str(&std::fs::read_to_string(&p).unwrap());
            }
        }
    }

    /// Every key the code uses is in English, and every translation only has English's
    /// keys, with the same `{values}`.
    #[test]
    fn every_key_is_translated_consistently() {
        let en = parse(LANGS[0].2);
        let en_bases: HashSet<&str> = en.keys().map(|k| base(k)).collect();
        let mut src = String::new();
        sources(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut src);
        // Keys always have a dot ("area.thing"); the macro's own docs don't.
        let re = regex::Regex::new(r#"t!\(\s*"([a-z0-9_]+\.[a-z0-9_.]+)""#).unwrap();
        let mut missing: Vec<String> = re.captures_iter(&src).map(|c| c[1].to_string()).filter(|k| !en_bases.contains(k.as_str())).collect();
        missing.sort();
        missing.dedup();
        assert!(missing.is_empty(), "keys missing from en.txt: {missing:?}");
        for (code, _, text) in &LANGS[1..] {
            let t = parse(text);
            for (k, v) in &t {
                let b = base(k);
                assert!(en_bases.contains(b), "{code}: {k} isn't an English key");
                let english = en.get(k).or_else(|| en.get(format!("{b}.other").as_str())).or_else(|| en.get(b)).unwrap();
                let (mut want, got) = (holes(english), holes(v));
                // A plural form may leave the count out ("one player"), never add one.
                if b != *k {
                    want.insert("n".into());
                }
                assert!(got.is_subset(&want), "{code}: {k} has {got:?}, English {want:?}");
            }
        }
    }

    /// Every language has every English key, so nothing shows in English by accident:
    /// a key added in English needs its translations in the same change.
    #[test]
    fn every_language_has_every_key() {
        let en: HashSet<&str> = parse(LANGS[0].2).keys().map(|k| base(k)).collect();
        let mut gaps = Vec::new();
        for (code, _, text) in &LANGS[1..] {
            let have: HashSet<&str> = parse(text).keys().map(|k| base(k)).collect();
            let mut missing: Vec<&str> = en.difference(&have).copied().collect();
            missing.sort();
            if !missing.is_empty() {
                gaps.push(format!("{code}: {missing:?}"));
            }
        }
        assert!(gaps.is_empty(), "untranslated keys:\n{}", gaps.join("\n"));
    }

    #[test]
    fn plural_forms() {
        assert_eq!(plural("en", 1), "one");
        assert_eq!(plural("en", 0), "other");
        assert_eq!(plural("fr", 0), "one");
        assert_eq!(plural("ru", 21), "one");
        assert_eq!(plural("ru", 3), "few");
        assert_eq!(plural("ru", 12), "many");
        assert_eq!(plural("pl", 22), "few");
        assert_eq!(plural("pl", 5), "many");
        assert_eq!(plural("ja", 1), "other");
    }

    #[test]
    fn system_tags_find_their_language() {
        let code = |t: &str| closest(t).map(|i| LANGS[i].0);
        assert_eq!(code("de-AT"), Some("de"));
        assert_eq!(code("zh-Hant-HK"), Some("zh-TW"));
        assert_eq!(code("zh-Hans-CN"), Some("zh-CN"));
        assert_eq!(code("zh-SG"), Some("zh-CN"));
        assert_eq!(code("pt-PT"), Some("pt-BR"));
        assert_eq!(code("en_GB"), Some("en"));
        assert_eq!(code("xx"), None);
    }

    #[test]
    fn values_and_plurals_fill_in() {
        CURRENT.store(0, Ordering::Relaxed);
        assert!(crate::t!("overlay.players_here", n = 1).contains('1'));
        assert_ne!(crate::t!("overlay.players_here", n = 1), crate::t!("overlay.players_here", n = 2));
    }
}
