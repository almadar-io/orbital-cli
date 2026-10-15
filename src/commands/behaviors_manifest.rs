//! Reader for the legacy JSON embedding manifests (`behaviors-embeddings.json`,
//! `knob-embeddings.json`). The catalog no longer reads them; `orb behaviors vectors import`
//! converts them into the vector store.

use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;

// ---------------------------------------------------------------------------

/// The keys of an embeddings file an import needs; every other entry is skipped while the file is
/// scanned. Keys are `<behavior>` and `<behavior>#capabilities` for an organism,
/// `<behavior>/<orbital>/<trait>/<knob>` for a knob (not imported: D-A14).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Want {
    Organisms,
}

impl Want {
    fn admits(&self, key: &str) -> bool {
        match self {
            Want::Organisms => !key.contains('/'),
        }
    }
}

/// The wanted part of one `orb.embeddings` file: the model it was stamped with and the selected
/// vectors, kept undecoded until asked for. Two on-disk shapes (mirroring
/// `behaviors/embeddings.ts` and `behaviors/knob-embeddings.ts`): `vectors` keyed directly, or an
/// interned `table` + `keys` index, which a read resolves for the wanted keys.
pub(crate) struct Manifest {
    pub(crate) model: String,
    vectors: HashMap<String, Value>,
}

#[derive(Default)]
struct Scanned {
    model: Option<String>,
    vectors: HashMap<String, Value>,
    keys: HashMap<String, usize>,
    saw_vectors: bool,
    saw_table: bool,
}

/// Pass 1 collects the model, the wanted direct vectors and the wanted `keys` entries; pass 2
/// (an interned file only) collects the wanted `table` rows.
enum Pass<'a> {
    Index(&'a Want, &'a mut Scanned),
    Table(&'a HashSet<usize>, &'a mut HashMap<usize, Value>),
}

impl<'de> DeserializeSeed<'de> for Pass<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for Pass<'_> {
    type Value = ();

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("an embeddings manifest object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        match self {
            Pass::Index(want, scanned) => {
                while let Some(field) = map.next_key::<String>()? {
                    match field.as_str() {
                        "model" => scanned.model = Some(map.next_value()?),
                        "vectors" => {
                            scanned.saw_vectors = true;
                            map.next_value_seed(Filtered {
                                want,
                                out: &mut scanned.vectors,
                            })?;
                        }
                        "keys" => {
                            map.next_value_seed(Filtered {
                                want,
                                out: &mut scanned.keys,
                            })?;
                        }
                        "table" => {
                            scanned.saw_table = true;
                            map.next_value::<IgnoredAny>()?;
                        }
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
            }
            Pass::Table(wanted, out) => {
                while let Some(field) = map.next_key::<String>()? {
                    if field == "table" {
                        map.next_value_seed(Rows {
                            wanted,
                            out: &mut *out,
                        })?;
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// A JSON object read into `out`, keeping only the entries whose key `want` admits.
struct Filtered<'a, V> {
    want: &'a Want,
    out: &'a mut HashMap<String, V>,
}

impl<'de, V: serde::Deserialize<'de>> DeserializeSeed<'de> for Filtered<'_, V> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_map(self)
    }
}

impl<'de, V: serde::Deserialize<'de>> Visitor<'de> for Filtered<'_, V> {
    type Value = ();

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("an object keyed by embedding key")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(key) = map.next_key::<String>()? {
            if self.want.admits(&key) {
                self.out.insert(key, map.next_value()?);
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(())
    }
}

/// The interned `table` array, keeping only the rows at the wanted indices.
struct Rows<'a> {
    wanted: &'a HashSet<usize>,
    out: &'a mut HashMap<usize, Value>,
}

impl<'de> DeserializeSeed<'de> for Rows<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for Rows<'_> {
    type Value = ();

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("an array of stored vectors")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        let mut index = 0usize;
        loop {
            if self.wanted.contains(&index) {
                match seq.next_element::<Value>()? {
                    Some(row) => {
                        self.out.insert(index, row);
                    }
                    None => break,
                }
            } else if seq.next_element::<IgnoredAny>()?.is_none() {
                break;
            }
            index += 1;
        }
        Ok(())
    }
}

impl Manifest {
    /// Streams the file once (twice when it is interned and a wanted key points into the table), so
    /// memory follows the wanted entries, not the file: an io knob file is hundreds of megabytes.
    pub(crate) fn read(path: &Path, want: &Want) -> anyhow::Result<Self> {
        let open = || -> anyhow::Result<_> {
            let file = std::fs::File::open(path)
                .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", path.display()))?;
            Ok(std::io::BufReader::with_capacity(1 << 20, file))
        };
        let invalid = |e: serde_json::Error| {
            anyhow::anyhow!("invalid embeddings manifest {}: {e}", path.display())
        };
        let mut scanned = Scanned::default();
        let mut de = serde_json::Deserializer::from_reader(open()?);
        Pass::Index(want, &mut scanned)
            .deserialize(&mut de)
            .and_then(|()| de.end())
            .map_err(invalid)?;
        let model = scanned
            .model
            .ok_or_else(|| anyhow::anyhow!("{} carries no `model`", path.display()))?;
        if !scanned.saw_vectors && !scanned.saw_table {
            anyhow::bail!(
                "{} carries neither `vectors` nor `table`/`keys`",
                path.display()
            );
        }
        let mut vectors = scanned.vectors;
        if scanned.saw_table && !scanned.keys.is_empty() {
            let wanted: HashSet<usize> = scanned.keys.values().copied().collect();
            let mut rows = HashMap::new();
            let mut de = serde_json::Deserializer::from_reader(open()?);
            Pass::Table(&wanted, &mut rows)
                .deserialize(&mut de)
                .and_then(|()| de.end())
                .map_err(invalid)?;
            for (key, index) in scanned.keys {
                if let Some(row) = rows.get(&index) {
                    vectors.entry(key).or_insert_with(|| row.clone());
                }
            }
        }
        Ok(Self { model, vectors })
    }

    pub(crate) fn vector(&self, key: &str) -> Option<Vec<f64>> {
        decode_vector(self.vectors.get(key)?)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.vectors.len()
    }
}

/// A plain number array, or `{ s, d }`: `d` = base64 of one signed byte per
/// dimension, value = `byte * s / 127`.
fn decode_vector(stored: &Value) -> Option<Vec<f64>> {
    match stored {
        Value::Array(items) => items.iter().map(Value::as_f64).collect(),
        Value::Object(obj) => {
            let scale = obj.get("s")?.as_f64()?;
            let bytes = base64_decode(obj.get("d")?.as_str()?)?;
            Some(
                bytes
                    .into_iter()
                    .map(|b| (f64::from(b as i8) * scale) / 127.0)
                    .collect(),
            )
        }
        _ => None,
    }
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in input.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            b'\n' | b'\r' | b' ' => continue,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

impl Manifest {
    pub(crate) fn keys(&self) -> impl Iterator<Item = &String> {
        self.vectors.keys()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_keeps_only_organism_and_capability_keys() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mixed.json");
        std::fs::write(
            &file,
            r#"{"model":"m","vectors":{"a":[1.0],"a#capabilities":[2.0],"a/O/T/k":[3.0],"b/O/T/k":[6.0]}}"#,
        )
        .unwrap();
        let m = Manifest::read(&file, &Want::Organisms).unwrap();
        assert_eq!(m.vector("a"), Some(vec![1.0]));
        assert_eq!(m.vector("a#capabilities"), Some(vec![2.0]));
        // Control: knob keys are not read.
        assert_eq!(m.vector("a/O/T/k"), None);
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn an_interned_file_resolves_only_the_wanted_keys_through_its_table() {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in [
            (
                "keys-first.json",
                r#"{"model":"m","keys":{"a":1,"a#capabilities":0,"b/O/T/k":2},"table":[[1.0],[2.0],[3.0]]}"#,
            ),
            (
                "table-first.json",
                r#"{"model":"m","table":[[1.0],[2.0],[3.0]],"keys":{"a":1,"a#capabilities":0,"b/O/T/k":2}}"#,
            ),
        ] {
            let file = dir.path().join(name);
            std::fs::write(&file, body).unwrap();
            let m = Manifest::read(&file, &Want::Organisms).unwrap();
            assert_eq!(m.vector("a"), Some(vec![2.0]), "{name}");
            assert_eq!(m.vector("a#capabilities"), Some(vec![1.0]), "{name}");
            assert_eq!(m.vector("b/O/T/k"), None, "{name}");
            assert_eq!(m.len(), 2, "{name}");
        }
    }

    #[test]
    fn a_direct_vector_wins_over_the_table_and_quantized_vectors_decode() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("both.json");
        std::fs::write(
            &file,
            r#"{"model":"m","vectors":{"a":[9.0],"q":{"s":1.0,"d":"fwA="}},"keys":{"a":0},"table":[[1.0]]}"#,
        )
        .unwrap();
        let m = Manifest::read(&file, &Want::Organisms).unwrap();
        assert_eq!(m.vector("a"), Some(vec![9.0]));
        assert_eq!(m.vector("q"), Some(vec![1.0, 0.0]));
    }

    #[test]
    fn a_file_with_no_vectors_or_table_is_refused_but_an_empty_selection_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let bare = dir.path().join("bare.json");
        std::fs::write(&bare, r#"{"model":"m"}"#).unwrap();
        let err = Manifest::read(&bare, &Want::Organisms)
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.contains("neither `vectors` nor `table`/`keys`"),
            "{err}"
        );
        let no_model = dir.path().join("no-model.json");
        std::fs::write(&no_model, r#"{"vectors":{"a":[1.0]}}"#).unwrap();
        let err = Manifest::read(&no_model, &Want::Organisms)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("carries no `model`"), "{err}");
        let nothing_wanted = dir.path().join("other.json");
        std::fs::write(
            &nothing_wanted,
            r#"{"model":"m","vectors":{"z/O/T/k":[1.0]}}"#,
        )
        .unwrap();
        let m = Manifest::read(&nothing_wanted, &Want::Organisms).unwrap();
        assert_eq!(m.len(), 0);
        assert_eq!(m.model, "m");
    }
}
