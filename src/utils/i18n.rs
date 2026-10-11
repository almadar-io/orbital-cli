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
            let value: serde_json::Value = serde_json::from_str(content).ok()?;
            let map = value.as_object()?;
            let lang =
                orbital_core::i18n::lang::for_orb_top_level_keys(map.keys().map(String::as_str))?;
            Some(lang.code.to_string())
        }
    }
}


/// Translate `.orb` JSON content from a native language to English via the
/// structural translator (`orbital_core::i18n::translate::translate_orb_value`).
/// Non-JSON content (`.lolo` source — the parser handles that natively, no
/// translation needed) and already-English/undetected JSON pass through
/// byte-identical.
pub(crate) fn translate_schema_content(content: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(content) else {
        return content.to_string();
    };
    let lang = match &value {
        serde_json::Value::Object(map) => {
            orbital_core::i18n::lang::for_orb_top_level_keys(map.keys().map(String::as_str))
        }
        _ => None,
    };
    let Some(lang) = lang else {
        return content.to_string();
    };
    eprintln!("Detected language: {} ({})", lang.def.meta.name, lang.code);
    orbital_core::i18n::translate::translate_orb_value(&mut value, lang);
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| content.to_string())
}
