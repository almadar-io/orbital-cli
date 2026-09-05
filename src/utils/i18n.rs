//! i18n helpers: language detection, locale copying, schema translation.
//! Both detection and translation are structural (JSON-key-based for `.orb`,
//! parser-driven for `.lolo`) — owned by `orbital_core::i18n` /
//! `orbital_lolo::detect_language`, never string/substring heuristics.

use std::path::Path;

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

/// Copy locale JSON files from {schema_dir}/locales/ into {output}/locales/.
/// The generated App.tsx imports from '../../../locales/{locale}.json' which resolves
/// to {output}/locales/ (3 levels up from packages/client/src/).
pub(crate) fn copy_locales(schema_path: &Path, output: &Path) {
    let schema_dir = match schema_path.parent() {
        Some(d) => d.to_path_buf(),
        None => return,
    };
    let src_locales = schema_dir.join("locales");
    let dest_locales = output.join("locales");
    if let Err(e) = std::fs::create_dir_all(&dest_locales) {
        eprintln!("Warning: could not create locales dir in output: {}", e);
        return;
    }

    if src_locales.exists() {
        // Copy locale files from source
        let entries = match std::fs::read_dir(&src_locales) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("json") {
                let dest = dest_locales.join(entry.file_name());
                if let Err(e) = std::fs::copy(&p, &dest) {
                    eprintln!("Warning: could not copy locale file {:?}: {}", p, e);
                }
            }
        }
    }

    // Ensure en/ar/sl locale files exist (fallback for standalone behaviors
    // that have no locales directory but the generated App.tsx imports them)
    for lang in &["en", "ar", "sl"] {
        let locale_path = dest_locales.join(format!("{}.json", lang));
        if !locale_path.exists() {
            if let Err(e) = std::fs::write(&locale_path, "{}") {
                eprintln!(
                    "Warning: could not create fallback locale {}.json: {}",
                    lang, e
                );
            }
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
