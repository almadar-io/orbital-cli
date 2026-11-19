//! i18n helpers: language detection, schema translation.
//! Both detection and translation are structural (JSON-key-based for `.orb`,
//! parser-driven for `.lolo`) — owned by `orbital_core::i18n` /
//! `orbital_lolo::detect_language`, never string/substring heuristics.

/// Detect the language of `content`. `.lolo` source is detected by the
/// parser itself (`orbital_lolo::detect_language`, always returns a code,
/// `"en"` by default). `.orb` JSON is detected structurally from its
/// top-level keys (`Lang::for_orb_top_level_keys`); returns `None` when the
/// content isn't valid JSON or no non-English language's `orbitals` key is
/// present among the top-level keys (i.e. English / undetected).
pub(crate) fn detect_language(content: &str) -> Option<String> {
    match crate::utils::detect_syntax(content) {
        crate::utils::InputSyntax::Lolo => Some(orbital_lolo::detect_language(content).to_string()),
        crate::utils::InputSyntax::OrbJson => {
            let keys = orb_top_level_keys(content)?;
            let lang = orbital_core::i18n::lang::for_orb_top_level_keys(keys.iter().map(String::as_str))?;
            Some(lang.code.to_string())
        }
    }
}


/// The top-level keys of `.orb` JSON `content`, read without building its tree (a program is tens of megabytes and
/// its language is told by these keys alone); `None` when the content is not a JSON object.
fn orb_top_level_keys(content: &str) -> Option<Vec<String>> {
    struct TopKeys(Vec<String>);
    impl<'de> serde::Deserialize<'de> for TopKeys {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct Keys;
            impl<'de> serde::de::Visitor<'de> for Keys {
                type Value = TopKeys;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("a JSON object")
                }
                fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<TopKeys, A::Error> {
                    let mut keys = Vec::new();
                    while let Some(key) = map.next_key::<String>()? {
                        map.next_value::<serde::de::IgnoredAny>()?;
                        keys.push(key);
                    }
                    Ok(TopKeys(keys))
                }
            }
            deserializer.deserialize_map(Keys)
        }
    }
    orbital_core::json_value::from_json_str::<TopKeys>(content).ok().map(|keys| keys.0)
}

/// Translate `.orb` JSON content from a native language to English via the
/// structural translator (`orbital_core::i18n::translate::translate_orb_value`).
/// Non-JSON content (`.lolo` source — the parser handles that natively, no
/// translation needed) and already-English/undetected JSON pass through
/// byte-identical.
pub(crate) fn translate_schema_content(content: &str) -> String {
    let lang = orb_top_level_keys(content)
        .and_then(|keys| orbital_core::i18n::lang::for_orb_top_level_keys(keys.iter().map(String::as_str)));
    let Some(lang) = lang else {
        return content.to_string();
    };
    let Ok(mut value) = orbital_core::json_value::from_json_str::<serde_json::Value>(content) else {
        return content.to_string();
    };
    eprintln!("Detected language: {} ({})", lang.def.meta.name, lang.code);
    orbital_core::i18n::translate::translate_orb_value(&mut value, lang);
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| content.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_top_level_keys_are_read_in_order_and_nested_keys_are_not() {
        let keys = orb_top_level_keys(r#"{ "name": "a", "orbitals": [ { "inner": { "deep": [1, 2, { "x": null }] } } ], "version": "1.0.0" }"#);
        assert_eq!(keys, Some(vec!["name".to_string(), "orbitals".to_string(), "version".to_string()]));
        assert_eq!(orb_top_level_keys("[1, 2]"), None, "control: an array has no keys");
        assert_eq!(orb_top_level_keys("{ \"name\": "), None, "control: broken JSON has no keys");
    }

    #[test]
    fn english_content_passes_through_byte_identical() {
        let content = "{ \"name\":   \"a\",\n \"orbitals\": [] }";
        assert_eq!(translate_schema_content(content), content);
        assert_eq!(translate_schema_content("not json"), "not json", "control: non-JSON passes through");
    }
}
