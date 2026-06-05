//! String-shape helpers.

/// Convert kebab-case to PascalCase (e.g., "signature-capture" → "SignatureCapture")
pub(crate) fn kebab_to_pascal(s: &str) -> String {
    s.split('-')
        .map(|part| {
            let mut c = part.chars();
            match c.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            }
        })
        .collect()
}
