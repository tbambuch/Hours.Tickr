//! All user-visible strings. The dictionaries live in `locales/*.json` and are
//! shared with the UI: the app sends the active one to the (sandboxed) windows over IPC.
//! Logs stay English and are not translated here.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde_json::Value;

pub const LANGUAGES: [&str; 2] = ["cs", "en"];

const CS: &str = include_str!("../../../locales/cs.json");
const EN: &str = include_str!("../../../locales/en.json");

struct Dicts {
    json: HashMap<&'static str, Value>,
    flat: HashMap<&'static str, HashMap<String, String>>,
}

fn dicts() -> &'static Dicts {
    static D: OnceLock<Dicts> = OnceLock::new();
    D.get_or_init(|| {
        let mut json = HashMap::new();
        let mut flat = HashMap::new();
        for (lang, raw) in [("cs", CS), ("en", EN)] {
            let v: Value = serde_json::from_str(raw).expect("locales JSON is embedded at build time and must be valid");
            let map = v
                .as_object()
                .map(|o| o.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect())
                .unwrap_or_default();
            flat.insert(lang, map);
            json.insert(lang, v);
        }
        Dicts { json, flat }
    })
}

/// Dictionary of `lang` as JSON (`en` for an unknown language).
pub fn dictionary(lang: &str) -> &'static Value {
    let d = dicts();
    d.json.get(lang).unwrap_or_else(|| &d.json["en"])
}

/// `pref`: "system" | "cs" | "en"; `system_langs`: preferred system languages, most preferred first.
/// `cs*` and `sk*` map to Czech, anything else (and an empty list) to English.
pub fn resolve_language(pref: &str, system_langs: &[String]) -> &'static str {
    if let Some(l) = LANGUAGES.iter().find(|l| **l == pref) {
        return l;
    }
    match system_langs.first().map(|s| s.to_lowercase()) {
        Some(first) if first.starts_with("cs") || first.starts_with("sk") => "cs",
        _ => "en",
    }
}

/// Looks `key` up in `lang`, then in English, then returns the key itself. `{name}` placeholders are filled from `params`;
/// unknown placeholders stay as they are.
pub fn translate(lang: &str, key: &str, params: &[(&str, &str)]) -> String {
    let d = dicts();
    let raw = d.flat.get(lang).and_then(|m| m.get(key)).or_else(|| d.flat["en"].get(key)).map(String::as_str).unwrap_or(key);
    if params.is_empty() {
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) if after[..end].chars().all(|c| c.is_alphanumeric() || c == '_') && end > 0 => {
                let name = &after[..end];
                match params.iter().find(|(k, _)| *k == name) {
                    Some((_, v)) => out.push_str(v),
                    None => out.push_str(&rest[start..start + end + 2]),
                }
                rest = &after[end + 1..];
            }
            _ => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(lang: &str) -> Vec<String> {
        let mut k: Vec<String> = dictionary(lang).as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    }

    #[test]
    fn both_languages_have_identical_keys() {
        assert_eq!(keys("cs"), keys("en"));
    }

    #[test]
    fn no_empty_values() {
        for lang in LANGUAGES {
            for (k, v) in dictionary(lang).as_object().unwrap() {
                assert!(!v.as_str().unwrap_or("").is_empty(), "{lang}:{k} is empty");
            }
        }
    }

    #[test]
    fn placeholders_match_between_languages() {
        let re = regex::Regex::new(r"\{(\w+)\}").unwrap();
        for (k, cs) in dictionary("cs").as_object().unwrap() {
            let en = dictionary("en")[k].as_str().unwrap();
            let mut a: Vec<_> = re.captures_iter(cs.as_str().unwrap()).map(|c| c[1].to_string()).collect();
            let mut b: Vec<_> = re.captures_iter(en).map(|c| c[1].to_string()).collect();
            a.sort();
            b.sort();
            assert_eq!(a, b, "placeholders differ for {k}");
        }
    }

    fn l(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn language_follows_the_system_unless_chosen() {
        assert_eq!(resolve_language("system", &l(&["cs-CZ"])), "cs");
        assert_eq!(resolve_language("system", &l(&["sk"])), "cs");
        assert_eq!(resolve_language("system", &l(&["de-DE"])), "en");
        assert_eq!(resolve_language("system", &[]), "en");
        assert_eq!(resolve_language("en", &l(&["cs-CZ"])), "en");
        assert_eq!(resolve_language("cs", &l(&["en-US"])), "cs");
        assert_eq!(resolve_language("bogus", &l(&["cs"])), "cs");
    }

    #[test]
    fn translate_fills_params_and_falls_back() {
        assert_eq!(translate("en", "tray.tooltip", &[("time", "3:07"), ("state", "tracking")]), "TimeWatch – 3:07 today (tracking)");
        assert_eq!(translate("cs", "timer.sync.pending", &[("n", "2")]), "Čeká na odeslání: 2");
        assert_eq!(translate("cs", "no.such.key", &[]), "no.such.key");
        assert_eq!(translate("xx", "tray.quit", &[]), "Quit");
        // unknown placeholder is left alone
        assert_eq!(translate("en", "timer.sync.pending", &[("other", "1")]), "Waiting to upload: {n}");
    }
}
