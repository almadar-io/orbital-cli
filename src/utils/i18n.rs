//! i18n helpers: language detection, locale copying, schema translation.

use std::path::PathBuf;

/// Heuristic to detect language from content
pub(crate) fn detect_language(content: &str) -> Option<String> {
    let definitions = crate::i18n::get_definitions();

    // Check purely based on unique keyword presence
    // This is a simple heuristic. A more robust way would be score-based.
    for (code, def) in definitions {
        if code == "en" {
            continue;
        }

        // check for "orbitals" keyword first (most common root key)
        if let Some(kw) = def.keywords.get("orbitals") {
            if content.contains(&format!("\"{}\"", kw)) {
                return Some(code);
            }
        }

        // check for "entity" keyword
        if let Some(kw) = def.keywords.get("entity") {
            if content.contains(&format!("\"{}\"", kw)) {
                return Some(code);
            }
        }

        // check for "orbital" keyword
        if let Some(kw) = def.keywords.get("orbital") {
            if content.contains(&format!("\"{}\"", kw)) {
                return Some(code);
            }
        }
    }

    None
}

/// Copy locale JSON files from {schema_dir}/locales/ into {output}/locales/.
/// The generated App.tsx imports from '../../../locales/{locale}.json' which resolves
/// to {output}/locales/ (3 levels up from packages/client/src/).
pub(crate) fn copy_locales(schema_path: &PathBuf, output: &PathBuf) {
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

/// Translate schema content from native language to English
pub(crate) fn translate_schema_content(content: &str) -> String {
    if let Some(lang) = detect_language(content) {
        let definitions = crate::i18n::get_definitions();
        if let Some(def) = definitions.get(&lang) {
            eprintln!("Detected language: {} ({})", def.meta.name, lang);
            let mut translated = content.to_string();

            // 1. Translate Keys
            for (eng_key, native_key) in &def.keywords {
                let target = format!("\"{}\":", eng_key);
                let source = format!("\"{}\":", native_key);
                translated = translated.replace(&source, &target);
            }

            // 2. Translate Operators (e.g. "يساوي" -> "eq")
            if let Some(ops) = &def.operators {
                for (eng_op, native_op) in ops {
                    let target = format!("\"{}\"", eng_op);
                    let source = format!("\"{}\"", native_op);
                    translated = translated.replace(&source, &target);
                }
            }

            // 3. Translate Bindings (e.g. "@كيان" -> "@entity")
            // This must handle path access like @كيان.رسالة -> @entity.message
            // But we only translate the ROOT binding name. The fields remain as defined in schema?
            // Actually, the schema fields might be localized too if "name" is localized.
            // But for now, let's just translate the root binding prefixes.
            if let Some(bindings) = &def.bindings {
                for (eng_binding, native_binding) in bindings {
                    // We must be careful not to replace partial matches if prefixes overlap (e.g. @ent vs @entity)
                    // But these designated bindings start with @ so it's safer.
                    // We simply replace all occurrences of the native binding prefix.
                    translated = translated.replace(native_binding, eng_binding);
                }
            }

            // 2. Translate Types (e.g. "type": "نص" -> "type": "string")
            // This is trickier because we need to match the value.
            // We can iterate over types and replace generic string matches if they look like types?
            // Or better: iterate over the *English* type names, finding their native equivalents and replacing them *only if* they appear as values for "type".
            // That requires parsing... which we can't do if it's mixed language.
            // Let's do a rigorous string replacement for types appearing in quotes.
            for (eng_type, native_type) in &def.types {
                // "type": "نص" -> "type": "string"
                // We can't rely on whitespace.
                // Let's just replacing the value string itself if it's unique enough?
                // "نص" is short. Might clobber user data.
                // SAFE APPROACH: We only translate keys for now as proof of concept.
                // Types might need to be exact matches.

                // Let's try replacing `"type": "NATIVE"` with `"type": "ENG"`
                // We need to account for possible whitespace.
                let patterns = vec![
                    format!("\"type\": \"{}\"", native_type),
                    format!("\"type\":  \"{}\"", native_type), // double space
                    format!("\"type\" : \"{}\"", native_type),
                ];

                for p in patterns {
                    translated = translated.replace(&p, &format!("\"type\": \"{}\"", eng_type));
                }
            }

            return translated;
        }
    }

    content.to_string()
}
