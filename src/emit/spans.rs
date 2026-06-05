//! JSON span scanner for `.orb` files.
//!
//! Walks an `.orb` JSON source once and records byte ranges for each
//! top-level node we care about: entities, traits, pages, services.
//! The ranges are whole-object (from `{` through matching `}`) — coarse
//! but enough for file-level source maps.
//!
//! Hand-rolled; no JSON parser dep beyond what's already used. Assumes
//! well-formed JSON (the `.orb` has already been validated by
//! `parse_schema` before we get here).
//!
//! ## Index shape
//!
//! ```text
//! OrbSpans {
//!   entities: [(entity_name, 1420..2108), ...],
//!   traits:   [(trait_name,  2110..5840), ...],
//!   pages:    [(page_path,   5842..6100), ...],
//! }
//! ```
//!
//! Used by `emit/sourcemap.rs` to build Source Map v3 mappings from
//! generated files back to the `.orb` source.

use std::collections::HashMap;
use std::ops::Range;

/// Byte ranges of top-level nodes in a `.orb` source.
#[derive(Debug, Default, Clone)]
pub struct OrbSpans {
    /// Entity name → byte range of its enclosing `{…}`.
    pub entities: HashMap<String, Range<usize>>,
    /// Trait name → byte range of its enclosing `{…}`.
    pub traits: HashMap<String, Range<usize>>,
    /// Page path → byte range of its enclosing `{…}`.
    pub pages: HashMap<String, Range<usize>>,
    /// Service name → byte range of its enclosing `{…}`.
    pub services: HashMap<String, Range<usize>>,
}

impl OrbSpans {
    /// Look up the span for a whole-file selector match.
    ///
    /// The emit filter's section logic categorizes each generated file as
    /// belonging to an entity, a trait, a page, or a service by grep'ing
    /// the path. Given that categorization, resolve the span.
    pub fn lookup(&self, kind: NodeKind, name: &str) -> Option<Range<usize>> {
        match kind {
            NodeKind::Entity => self.entities.get(name).cloned(),
            NodeKind::Trait => self.traits.get(name).cloned(),
            NodeKind::Page => self.pages.get(name).cloned(),
            NodeKind::Service => self.services.get(name).cloned(),
        }
    }
}

/// Which top-level node kind a generated file corresponds to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Entity,
    Trait,
    Page,
    Service,
}

/// Scan an `.orb` JSON source for top-level node byte ranges.
///
/// Returns an empty `OrbSpans` rather than an error if the source is not
/// recognizably an `.orb` JSON (emit already surfaces parse errors upstream).
pub fn scan(source: &str) -> OrbSpans {
    let mut out = OrbSpans::default();
    let bytes = source.as_bytes();

    // We only care about four paths:
    //
    //   orbitals[*].entity          → entity
    //   orbitals[*].traits[*]       → trait (keyed by `name` field inside)
    //   orbitals[*].pages[*]        → page  (keyed by `path` field inside)
    //   orbitals[*].services.NAME   → service (object under a keyed container)
    //
    // The scanner tracks a stack of containers. When we enter a recognized
    // object value at a known path, we record (start, name) — then when we
    // exit at balanced depth we finalize the end.

    let mut i = 0usize;
    let mut path: Vec<PathSeg> = Vec::new();
    let mut pending: Vec<PendingNode> = Vec::new();

    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                // Key or string value — skip to end of string.
                let (end, contents) = scan_string(bytes, i);
                // If we're positioned *at* a key (we just saw `{` or `,` and
                // the next significant char is `"`), the string is a key
                // iff the next non-whitespace char after the string is `:`.
                let post = skip_ws(bytes, end + 1);
                if post < bytes.len() && bytes[post] == b':' {
                    // It's a key. Remember it; the next value belongs to it.
                    if let Some(last) = path.last_mut() {
                        last.set_pending_key(contents);
                    }
                    i = post + 1; // past the `:`
                    i = skip_ws(bytes, i);
                    continue;
                } else {
                    // It's a value — scalar string. Nothing to do for spans.
                    i = end + 1;
                    continue;
                }
            }
            b'{' => {
                let start = i;
                path.push(PathSeg::new_object());
                pending.push(PendingNode {
                    start,
                    is_tracked: is_tracked_object(&path),
                });
                i += 1;
                i = skip_ws(bytes, i);
            }
            b'}' => {
                let end_inclusive = i;
                if let Some(done) = pending.pop() {
                    if done.is_tracked {
                        finalize_tracked(&mut out, &path, source, done.start..end_inclusive + 1);
                    }
                }
                path.pop();
                i += 1;
                i = skip_ws(bytes, i);
                // When we close this value, advance the parent array index.
                if let Some(parent) = path.last_mut() {
                    parent.step_after_value();
                }
            }
            b'[' => {
                path.push(PathSeg::new_array());
                i += 1;
                i = skip_ws(bytes, i);
            }
            b']' => {
                path.pop();
                i += 1;
                i = skip_ws(bytes, i);
                if let Some(parent) = path.last_mut() {
                    parent.step_after_value();
                }
            }
            b',' => {
                i += 1;
                i = skip_ws(bytes, i);
            }
            c if c.is_ascii_whitespace() => {
                i = skip_ws(bytes, i);
            }
            // Scalars (numbers, true, false, null) — skip the token.
            _ => {
                let end = scan_scalar(bytes, i);
                i = end;
                if let Some(parent) = path.last_mut() {
                    parent.step_after_value();
                }
                i = skip_ws(bytes, i);
            }
        }
    }

    out
}

// --- Internal state --------------------------------------------------------

/// A single path segment recording the current parsing context.
///
/// For objects: the most recently seen key (`pending_key`) and the key
/// associated with the last-completed child value (`last_key`).
///
/// For arrays: the current element index, advanced by `step_after_value`.
#[derive(Debug, Clone)]
struct PathSeg {
    kind: SegKind,
    // For objects.
    pending_key: Option<String>,
    last_key: Option<String>,
    // For arrays.
    index: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SegKind {
    Object,
    Array,
}

impl PathSeg {
    fn new_object() -> Self {
        Self {
            kind: SegKind::Object,
            pending_key: None,
            last_key: None,
            index: 0,
        }
    }
    fn new_array() -> Self {
        Self {
            kind: SegKind::Array,
            pending_key: None,
            last_key: None,
            index: 0,
        }
    }
    fn set_pending_key(&mut self, k: String) {
        self.pending_key = Some(k);
    }
    fn step_after_value(&mut self) {
        match self.kind {
            SegKind::Object => {
                // Lock in the key that owned the just-completed value.
                self.last_key = self.pending_key.take();
            }
            SegKind::Array => {
                self.index += 1;
            }
        }
    }
}

/// A tracked open object whose end byte we'll fill in when we see the closing `}`.
#[derive(Debug, Clone)]
struct PendingNode {
    start: usize,
    is_tracked: bool,
}

/// Is the object we just entered one we care about?
///
/// Path shapes we track:
///   orbitals[*].entity                     → tracked (kind: Entity)
///   orbitals[*].traits[*]                  → tracked (kind: Trait)
///   orbitals[*].pages[*]                   → tracked (kind: Page)
///   orbitals[*].services.<NAME>            → tracked (kind: Service)
///
/// The path stack at the moment `{` of a tracked object is seen already
/// contains that object (just pushed by the scanner), plus its ancestors.
fn is_tracked_object(path: &[PathSeg]) -> bool {
    // Walk up from the end, skipping the top (which is the object itself).
    // The top-of-stack is the just-pushed Object.
    if path.len() < 3 {
        return false;
    }
    // Parent of the current object tells us "which container key does it live under?"
    // For e.g. `orbitals[0].traits[2]`:
    //   path = [Object("root"), Array(orbitals), Object(orbital), Array(traits), Object(trait — current)]
    // We're looking at the Object at the top; its parent is an Array; the
    // Array's parent is the Object whose pending_key was "traits".
    // For orbitals[0].entity: parent is Object with key "entity".
    // For services.NAME: parent is the services Object; grandparent is the
    // orbital Object with key "services".
    let me_idx = path.len() - 1;
    if path[me_idx].kind != SegKind::Object {
        return false;
    }
    let parent = &path[me_idx - 1];
    match parent.kind {
        SegKind::Array => {
            // Inside an array. Which array?
            if path.len() < 4 {
                return false;
            }
            let gp = &path[me_idx - 2]; // The object that owns the array.
            if gp.kind != SegKind::Object {
                return false;
            }
            let array_key = gp.pending_key.as_deref();
            matches!(array_key, Some("traits") | Some("pages"))
                && has_orbital_ancestor(path, me_idx - 3)
        }
        SegKind::Object => {
            // Inside an object keyed directly. Either `orbitals[i].entity`
            // or `orbitals[i].services.<NAME>`.
            let key = parent.pending_key.as_deref();
            match key {
                Some("entity") => has_orbital_ancestor(path, me_idx - 2),
                _ => {
                    // services.<NAME> — parent is the services container object,
                    // grandparent is the orbital with pending_key "services".
                    if path.len() < 4 {
                        return false;
                    }
                    let gp = &path[me_idx - 2];
                    matches!(gp.pending_key.as_deref(), Some("services"))
                        && has_orbital_ancestor(path, me_idx - 3)
                }
            }
        }
    }
}

fn has_orbital_ancestor(path: &[PathSeg], from: usize) -> bool {
    // Walk outward from `from` looking for an Object whose pending_key is
    // "orbitals". We don't need to verify it's at the root; there's no nested
    // orbitals in the schema.
    let mut i = from as isize;
    while i >= 0 {
        let seg = &path[i as usize];
        if seg.kind == SegKind::Object && seg.pending_key.as_deref() == Some("orbitals") {
            return true;
        }
        i -= 1;
    }
    // Also accept: we were in an array whose owner was "orbitals" — already
    // handled via pending_key check above since the scanner records
    // `pending_key = "orbitals"` on the root object just before entering the
    // Array.
    false
}

/// When a tracked object closes, extract its identifier and record the span.
fn finalize_tracked(
    out: &mut OrbSpans,
    path: &[PathSeg],
    source: &str,
    range: Range<usize>,
) {
    // Classify kind based on the path we're at.
    let me_idx = path.len() - 1;
    let parent = &path[me_idx - 1];
    let kind = match parent.kind {
        SegKind::Array => {
            let gp = &path[me_idx - 2];
            match gp.pending_key.as_deref() {
                Some("traits") => NodeKind::Trait,
                Some("pages") => NodeKind::Page,
                _ => return,
            }
        }
        SegKind::Object => match parent.pending_key.as_deref() {
            Some("entity") => NodeKind::Entity,
            _ => NodeKind::Service, // orbitals[i].services.<NAME>
        },
    };

    // Pull the identifier out of the range's JSON content.
    let slice = &source[range.clone()];
    let id_key = match kind {
        NodeKind::Page => "path",
        _ => "name",
    };
    let Some(name) = extract_simple_string_field(slice, id_key) else {
        return;
    };

    let target = match kind {
        NodeKind::Entity => &mut out.entities,
        NodeKind::Trait => &mut out.traits,
        NodeKind::Page => &mut out.pages,
        NodeKind::Service => &mut out.services,
    };
    target.insert(name, range);
}

/// Find the value of a top-level string field inside a JSON object slice.
///
/// Not a full JSON parser — uses substring search and string-literal
/// scanning. Returns `None` if the field doesn't appear or the value isn't
/// a string. Good enough for `"name"` and `"path"` which are always simple
/// top-level strings in an entity/trait/page/service object.
fn extract_simple_string_field(object_slice: &str, key: &str) -> Option<String> {
    let bytes = object_slice.as_bytes();
    let pattern = format!("\"{}\"", key);
    let mut i = 0usize;
    while i + pattern.len() < bytes.len() {
        // Find the next potential match.
        let idx = twoway_find(bytes, i, pattern.as_bytes())?;
        // Make sure it's at top level of the object: parentheses balanced
        // from start of slice through idx, and we're inside exactly one `{`.
        if is_top_level_key(bytes, idx) {
            // After the key, skip ws + `:` + ws.
            let mut j = idx + pattern.len();
            j = skip_ws(bytes, j);
            if j >= bytes.len() || bytes[j] != b':' {
                i = idx + pattern.len();
                continue;
            }
            j += 1;
            j = skip_ws(bytes, j);
            if j >= bytes.len() || bytes[j] != b'"' {
                // Not a string value.
                return None;
            }
            let (end, s) = scan_string(bytes, j);
            let _ = end;
            return Some(s);
        }
        i = idx + pattern.len();
    }
    None
}

/// Is `pos` a top-level key position within the object that starts at byte 0?
/// Top-level here means "brace depth == 1 and bracket depth == 0, and we are
/// not inside a string."
fn is_top_level_key(bytes: &[u8], pos: usize) -> bool {
    let mut depth_brace = 0i32;
    let mut depth_bracket = 0i32;
    let mut in_string = false;
    let mut escape = false;
    let mut i = 0usize;
    while i < pos {
        let c = bytes[i];
        if in_string {
            if escape {
                escape = false;
            } else if c == b'\\' {
                escape = true;
            } else if c == b'"' {
                in_string = false;
            }
        } else {
            match c {
                b'"' => in_string = true,
                b'{' => depth_brace += 1,
                b'}' => depth_brace -= 1,
                b'[' => depth_bracket += 1,
                b']' => depth_bracket -= 1,
                _ => {}
            }
        }
        i += 1;
    }
    depth_brace == 1 && depth_bracket == 0 && !in_string
}

/// Simple substring search starting at `from`.
fn twoway_find(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() - from {
        return None;
    }
    for i in from..=haystack.len() - needle.len() {
        if haystack[i..i + needle.len()] == *needle {
            return Some(i);
        }
    }
    None
}

/// Skip ASCII whitespace; return the index of the next non-ws byte (or `bytes.len()`).
fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// `bytes[i] == b'"'` assumed. Returns `(end_index_of_closing_quote, unescaped_contents)`.
fn scan_string(bytes: &[u8], i: usize) -> (usize, String) {
    debug_assert_eq!(bytes[i], b'"');
    let mut out = String::new();
    let mut j = i + 1;
    while j < bytes.len() {
        match bytes[j] {
            b'"' => return (j, out),
            b'\\' => {
                // Minimal unescaping: support \" \\ \n \t — otherwise pass byte through.
                j += 1;
                if j >= bytes.len() {
                    return (j, out);
                }
                let esc = bytes[j];
                let ch = match esc {
                    b'"' => '"',
                    b'\\' => '\\',
                    b'/' => '/',
                    b'n' => '\n',
                    b't' => '\t',
                    b'r' => '\r',
                    b'b' => '\u{0008}',
                    b'f' => '\u{000C}',
                    _ => esc as char,
                };
                out.push(ch);
                j += 1;
            }
            c => {
                out.push(c as char);
                j += 1;
            }
        }
    }
    (j, out)
}

/// Advance past a JSON scalar (number, true, false, null). Returns index
/// one past the last byte of the scalar.
fn scan_scalar(bytes: &[u8], from: usize) -> usize {
    let mut j = from;
    while j < bytes.len() {
        match bytes[j] {
            b',' | b']' | b'}' => break,
            c if c.is_ascii_whitespace() => break,
            _ => j += 1,
        }
    }
    j
}

// ---------------------------------------------------------------------------
// Byte-offset ↔ line/column conversion
// ---------------------------------------------------------------------------

/// Convert a byte offset into a 0-indexed (line, column) pair. Efficient
/// enough for the handful of lookups source-map emission needs.
pub fn byte_to_line_col(source: &str, offset: usize) -> (u32, u32) {
    let clamped = offset.min(source.len());
    let mut line = 0u32;
    let mut col = 0u32;
    for (i, ch) in source.char_indices() {
        if i >= clamped {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (line, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMA: &str = r#"{
  "name": "std-modal",
  "orbitals": [
    {
      "name": "ModalRecordOrbital",
      "entity": {
        "name": "ModalRecord",
        "fields": []
      },
      "traits": [
        {"name": "ModalRecordModal", "states": []}
      ],
      "pages": [
        {"path": "/modal", "name": "ModalPage"}
      ]
    }
  ]
}"#;

    #[test]
    fn scans_modal_schema() {
        let spans = scan(SCHEMA);

        // Entity
        let e = spans
            .entities
            .get("ModalRecord")
            .expect("entity span missing");
        assert!(SCHEMA[e.clone()].contains("\"ModalRecord\""));
        assert!(SCHEMA[e.clone()].starts_with('{'));
        assert!(SCHEMA[e.clone()].ends_with('}'));

        // Trait
        let t = spans
            .traits
            .get("ModalRecordModal")
            .expect("trait span missing");
        assert!(SCHEMA[t.clone()].contains("\"ModalRecordModal\""));

        // Page (keyed by `path`, not name)
        let p = spans.pages.get("/modal").expect("page span missing");
        assert!(SCHEMA[p.clone()].contains("/modal"));
    }

    #[test]
    fn byte_to_line_col_works() {
        let s = "line0\nline1\nline2";
        assert_eq!(byte_to_line_col(s, 0), (0, 0));
        assert_eq!(byte_to_line_col(s, 5), (0, 5));
        assert_eq!(byte_to_line_col(s, 6), (1, 0));
        assert_eq!(byte_to_line_col(s, 12), (2, 0));
    }

    #[test]
    fn empty_source_does_not_panic() {
        let _ = scan("");
        let _ = scan("{}");
    }
}
