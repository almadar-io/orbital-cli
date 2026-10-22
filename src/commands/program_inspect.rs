use super::program_host::failure;
use orbital_compiler::phases::inline::InlineSession;
use orbital_core::effects::executor::SubstrateFailure;
use orbital_core::schema::{OrbitalSchema, TraitReference};
use orbital_lolo::LoloSourceMap;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

struct Document {
    path: PathBuf,
    schema: OrbitalSchema,
    map: LoloSourceMap,
    source: String,
}

pub(crate) fn inspect(source: &str, options: &Value) -> Result<Value, SubstrateFailure> {
    let path = options
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| Path::new(p).is_absolute())
        .ok_or_else(|| {
            failure("program/read inspect: path must be the original absolute .lolo path")
        })?;
    if options
        .get("version")
        .is_some_and(|v| !v.is_null() && v.as_u64().is_none())
    {
        return Err(failure(
            "program/read inspect: version must be a nonnegative integer",
        ));
    }
    let (schema, map) = orbital_lolo::parse_and_lower_str_with_map(source, path)
        .map_err(|e| failure(format!("program/read inspect: {}", e.render_with_hint())))?;
    orbital_compiler::loader::register_source_lowerer(crate::lower_lolo_file);
    let mut compiler = orbital_compiler::CompilerOptions::from_env();
    let session = InlineSession::default();
    compiler.inline.session = Some(session.clone());
    let original = schema.clone();
    let authored_program = original.clone();
    let (program, lazy) = orbital_compiler::phases::lazy::resolve_program_with_path(
        schema,
        &compiler,
        Some(Path::new(path)),
    )
    .map_err(|e| failure(format!("program/read inspect: {e}")))?;
    let mut documents = BTreeMap::new();
    documents.insert(
        PathBuf::from(path),
        Document {
            path: path.into(),
            schema: original,
            map,
            source: source.to_string(),
        },
    );
    let mut imports = Vec::new();
    let mut imported = Vec::new();
    session.each_loaded_schema(&mut |loaded| {
        imports.push(
            json!({"path": std::fs::canonicalize(&loaded.source_path).unwrap_or_else(|_| loaded.source_path.clone()), "specifier": loaded.import_path,
            "program": loaded.schema.name, "authoredProgram": loaded.schema}),
        );
        imported.push(loaded.clone());
    });
    for loaded in imported {
        if loaded
            .source_path
            .extension()
            .is_some_and(|ext| ext == "lolo")
        {
            let text =
                std::fs::read_to_string(&loaded.source_path).map_err(|e| failure(e.to_string()))?;
            let (mapped_schema, map) = orbital_lolo::parse_and_lower_str_with_map(
                &text,
                &loaded.source_path.to_string_lossy(),
            )
            .map_err(|e| failure(e.render_with_hint()))?;
            if serde_json::to_value(&mapped_schema).map_err(|e| failure(e.to_string()))?
                != serde_json::to_value(&loaded.schema).map_err(|e| failure(e.to_string()))?
            {
                return Err(failure(format!(
                    "program/read inspect: imported source changed during inspection: {}",
                    loaded.source_path.display()
                )));
            }
            let actual_path =
                std::fs::canonicalize(&loaded.source_path).map_err(|e| failure(e.to_string()))?;
            documents.insert(
                actual_path.clone(),
                Document {
                    path: actual_path,
                    schema: loaded.schema,
                    source: text,
                    map,
                },
            );
        }
    }
    let origins = session.trait_origins();
    let root_document = documents
        .get(Path::new(path))
        .ok_or_else(|| failure("program/read inspect: original document is unavailable"))?;
    let composition = composition_bindings(&program, &authored_program, &origins, root_document)?;
    let mut anchors = Vec::new();
    collect_anchors(
        &program,
        &documents,
        &origins,
        Some(Path::new(path)),
        &mut anchors,
    );
    // Lazy programs have their own identities and pointer root, recorded explicitly.
    let mut lazy_maps = Vec::new();
    fn collect_lazy(
        items: &[orbital_compiler::phases::lazy::ResolvedLazy],
        documents: &BTreeMap<PathBuf, Document>,
        origins: &orbital_compiler::loader::SourceTraitOrigins,
        maps: &mut Vec<Value>,
    ) {
        for item in items {
            let mut anchors = Vec::new();
            collect_anchors(&item.schema, documents, origins, None, &mut anchors);
            maps.push(json!({"orbital": item.orbital, "orbRef": item.orb_ref, "program": item.schema, "sourceMap": anchors}));
            collect_lazy(&item.lazy, documents, origins, maps);
        }
    }
    collect_lazy(&lazy, &documents, &origins, &mut lazy_maps);
    anchors.sort_by(|a, b| a["pointer"].as_str().cmp(&b["pointer"].as_str()));
    let mut definitions = Vec::new();
    for orbital in &program.orbitals {
        for reference in &orbital.traits {
            if let TraitReference::Inline(definition) = reference {
                if let Some(id) = &definition.id {
                    let fingerprint = orbital_core::schema::trait_execution_fingerprint(definition)
                        .map_err(|e| failure(e.to_string()))?;
                    let paths: std::collections::BTreeSet<&str> = anchors
                        .iter()
                        .filter(|anchor| anchor["traitId"].as_str() == Some(id.as_str()))
                        .filter_map(|anchor| anchor["path"].as_str())
                        .collect();
                    let source_path = if paths.len() == 1 {
                        paths.first().copied()
                    } else {
                        None
                    };
                    let source = definition.source_declaration.as_ref().map(|source| json!({
                        "traitId": source.trait_id, "fingerprint": source.fingerprint, "path": source_path
                    }));
                    definitions.push(json!({"traitId": id, "definitionFingerprint": fingerprint,
                        "sourceDeclaration": source, "sourceCandidates": paths}));
                }
            }
        }
    }
    Ok(
        json!({"program": program, "authoredProgram": authored_program, "definitions": definitions, "composition": composition, "sourceMap": anchors, "imports": imports, "lazy": lazy_maps,
        "path": path, "version": options.get("version"), "columnEncoding": "unicode-scalar", "lengthEncoding": "utf-8-bytes", "lineBase": 1, "columnBase": 1}),
    )
}

fn composition_bindings(
    program: &OrbitalSchema,
    authored: &OrbitalSchema,
    origins: &orbital_compiler::loader::SourceTraitOrigins,
    root_document: &Document,
) -> Result<Vec<Value>, SubstrateFailure> {
    let root_source =
        std::fs::canonicalize(&root_document.path).unwrap_or_else(|_| root_document.path.clone());
    let mut bindings = Vec::new();
    for orbital in &program.orbitals {
        for tr in &orbital.traits {
            let Some(id) = tr.trait_decl_id() else {
                continue;
            };
            if let Some((oi, key)) = origins
                .get(&(root_source.clone(), id.to_string()))
                .and_then(|o| o.root_callsite.as_ref())
            {
                let source = authored.orbitals.get(*oi).ok_or_else(|| {
                    failure("program/read inspect: materialized callsite has no authored orbital")
                })?;
                let source_location = root_document
                    .map
                    .lookup_detailed(&format!("orbitals[{oi}].traits({key})"))
                    .and_then(|(span, exact)| source_location(root_document, span, exact));
                bindings.push(json!({"source": source_location, "traitId": id, "kind": "orbital-reference", "authoredPointer": format!("orbitals[{oi}].reference"),
                    "traitOverrideKey": key, "binding": source.reference}));
                continue;
            }
            for (oi, source_orbital) in authored.orbitals.iter().enumerate() {
                for (ti, source_trait) in source_orbital.traits.iter().enumerate() {
                    if source_trait.trait_decl_id() == Some(id) {
                        let source_location = root_document
                            .map
                            .lookup_detailed(&format!("orbitals[{oi}].traits[{ti}]"))
                            .and_then(|(span, exact)| source_location(root_document, span, exact));
                        bindings.push(json!({"source": source_location, "traitId": id, "kind": "trait-declaration", "authoredPointer": format!("orbitals[{oi}].traits[{ti}]"),
                            "traitOverrideKey": null, "binding": source_trait}));
                    }
                }
            }
        }
    }
    Ok(bindings)
}

fn source_location(doc: &Document, span: orbital_lolo::ast::Span, exact: bool) -> Option<Value> {
    let range = orbital_lolo::source_map::source_range(&doc.source, span)?;
    let mut location = json!({"path": doc.path, "line": span.line, "column": span.col,
        "length": range.byte_length, "endLine": range.end_line, "endColumn": range.end_column, "exact": exact});
    if let Some(scope) = orbital_lolo::source_map::source_scope_range(&doc.source, span) {
        location["scopeEndLine"] = json!(scope.end_line);
        location["scopeEndColumn"] = json!(scope.end_column);
        location["scopeLength"] = json!(scope.byte_length);
    }
    Some(location)
}

fn collect_anchors(
    program: &OrbitalSchema,
    documents: &BTreeMap<PathBuf, Document>,
    origins: &orbital_compiler::loader::SourceTraitOrigins,
    root_source: Option<&Path>,
    anchors: &mut Vec<Value>,
) {
    for (oi, orbital) in program.orbitals.iter().enumerate() {
        for (ti, tr) in orbital.traits.iter().enumerate() {
            let TraitReference::Inline(trait_def) = tr else {
                continue;
            };
            let origin = trait_def
                .source_declaration
                .as_ref()
                .map(|s| s.trait_id.as_str())
                .or(trait_def.id.as_deref());
            let Some(origin) = origin else { continue };
            let materialized = root_source.and_then(|path| {
                let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
                trait_def
                    .id
                    .as_ref()
                    .and_then(|id| origins.get(&(path, id.clone())))
            });
            let origin_path = materialized
                .and_then(|o| o.source_path.as_deref())
                .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()))
                .or_else(|| {
                    (trait_def.id.as_deref() == Some(origin))
                        .then(|| root_source.map(Path::to_path_buf))
                        .flatten()
                });
            let origin = materialized.map(|o| o.trait_id.as_str()).unwrap_or(origin);
            for doc in documents.values() {
                if origin_path.as_ref().is_some_and(|path| path != &doc.path) {
                    continue;
                }
                for (source_oi, source_orbital) in doc.schema.orbitals.iter().enumerate() {
                    for (source_ti, source_trait) in source_orbital.traits.iter().enumerate() {
                        if source_trait.trait_decl_id() != Some(origin) {
                            continue;
                        }
                        let base = format!("orbitals[{source_oi}].traits[{source_ti}]");
                        let target = format!("orbitals[{oi}].traits[{ti}]");
                        for (pointer, span) in doc.map.entries() {
                            if span.line == 0 || span.col == 0 {
                                continue;
                            }
                            let Some(suffix) = pointer.strip_prefix(&base) else {
                                continue;
                            };
                            if !suffix.is_empty() && !suffix.starts_with('.') {
                                continue;
                            }
                            let exact = suffix.is_empty()
                                || [".stateMachine.states[", ".stateMachine.transitions["]
                                    .iter()
                                    .any(|prefix| {
                                        suffix
                                            .strip_prefix(prefix)
                                            .and_then(|s| s.strip_suffix(']'))
                                            .is_some_and(|s| s.parse::<usize>().is_ok())
                                    });
                            if let Some(mut anchor) = source_location(doc, span, exact) {
                                anchor["pointer"] = json!(format!("{target}{suffix}"));
                                anchor["orbitalId"] = json!(orbital.id);
                                anchor["traitId"] = json!(trait_def.id);
                                anchor["originTraitId"] = json!(origin);
                                anchors.push(anchor);
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ATOM: &str = r#"app native-inspection-atom [version: "1.0.0"]
orbital Mechanism {
  entity Row [runtime] {
    id : string!
    text : string = ""
  }
  trait Cycle -> @rebindable Row [interaction] {
    initial: idle
    state idle {
      NEXT -> done
        (set @entity.text "🌱 calm")
    }
    state done { RESET -> idle }
  }
}
"#;

    #[test]
    fn inspect_uses_unsaved_original_and_unicode_scalar_anchors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("original.lolo");
        std::fs::write(&path, "this disk content is deliberately invalid").unwrap();
        let original = format!(";; 🌱 unsaved original\n{ATOM}");
        let reply = inspect(&original, &json!({"path":path,"version":17})).unwrap();
        assert_eq!(reply["version"], 17);
        let declaration = &reply["program"]["orbitals"][0]["traits"][0]["sourceDeclaration"];
        assert_eq!(
            reply["definitions"][0]["sourceDeclaration"]["traitId"],
            declaration["traitId"]
        );
        assert_eq!(
            reply["definitions"][0]["sourceDeclaration"]["fingerprint"],
            declaration["fingerprint"]
        );
        assert!(declaration.get("path").is_none());
        let edited = original.replace("🌱 calm", "🌱 vivid");
        let changed = inspect(&edited, &json!({"path":path,"version":18})).unwrap();
        assert_eq!(
            changed["definitions"][0]["sourceDeclaration"]["traitId"],
            declaration["traitId"]
        );
        assert_ne!(
            changed["definitions"][0]["sourceDeclaration"]["fingerprint"],
            declaration["fingerprint"]
        );
        assert_eq!(reply["columnEncoding"], "unicode-scalar");
        let anchors = reply["sourceMap"].as_array().unwrap();
        let arm = anchors
            .iter()
            .find(|a| a["pointer"] == "orbitals[0].traits[0].stateMachine.transitions[0]")
            .unwrap();
        assert_eq!(arm["path"], path.to_string_lossy().as_ref());
        assert_eq!(arm["exact"], true);
        assert!(arm["scopeLength"].as_u64().unwrap() > arm["length"].as_u64().unwrap());
        assert!(arm["scopeEndLine"].as_u64().unwrap() > arm["endLine"].as_u64().unwrap());
        let line = original
            .lines()
            .nth(arm["line"].as_u64().unwrap() as usize - 1)
            .unwrap();
        assert!(line
            .chars()
            .skip(arm["column"].as_u64().unwrap() as usize - 1)
            .collect::<String>()
            .starts_with("NEXT"));
        let state = anchors
            .iter()
            .find(|a| a["pointer"] == "orbitals[0].traits[0].stateMachine.states[0]")
            .unwrap();
        assert_eq!(state["exact"], true);
        let reordered_source = original.replace("initial: idle", "initial: done");
        let reordered = inspect(&reordered_source, &json!({"path":path})).unwrap();
        let moved = reordered["sourceMap"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["pointer"] == "orbitals[0].traits[0].stateMachine.states[0]")
            .unwrap();
        assert_eq!(
            reordered["program"]["orbitals"][0]["traits"][0]["stateMachine"]["states"][0]["name"],
            "done"
        );
        assert!(reordered_source
            .lines()
            .nth(moved["line"].as_u64().unwrap() as usize - 1)
            .unwrap()
            .contains("state done"));
        let effect = anchors
            .iter()
            .find(|a| {
                a["pointer"] == "orbitals[0].traits[0].stateMachine.transitions[0].effects[0]"
            })
            .unwrap();
        assert_eq!(
            effect["exact"], false,
            "expression rewrites must not claim exact post-resolution correspondence"
        );
    }

    #[test]
    fn inspect_composed_trait_and_orbital_follow_authoritative_original_ids() {
        let dir = tempfile::tempdir().unwrap();
        let atom = dir.path().join("atom.lolo");
        std::fs::write(&atom, ATOM).unwrap();
        let twin = dir.path().join("twin.lolo");
        std::fs::write(&twin, ATOM).unwrap();
        let middle = dir.path().join("middle.lolo");
        std::fs::write(
            &middle,
            r#"app middle-inspection [version: "1.0.0"]
uses A from "./atom"
orbital Middle {
  entity Item [runtime] {
    id : string!
    text : string = ""
  }
  trait Named = A.traits.Cycle -> Item {}
}
"#,
        )
        .unwrap();
        let wrapper = r#"app wrapper-inspection [version: "1.0.0"]
uses A from "./atom"
orbital Inner = A.orbitals.Mechanism {}
"#;
        std::fs::write(dir.path().join("wrapper.lolo"), wrapper).unwrap();
        let other = dir.path().join("other");
        std::fs::create_dir(&other).unwrap();
        std::fs::write(other.join("atom.lolo"), ATOM).unwrap();
        std::fs::write(other.join("wrapper.lolo"), wrapper).unwrap();
        let other_atom = std::fs::canonicalize(other.join("atom.lolo")).unwrap();
        let atom = std::fs::canonicalize(atom).unwrap();
        let twin = std::fs::canonicalize(twin).unwrap();
        let source = r#"app native-inspection-consumer [version: "1.0.0"]
uses A from "./atom"
uses B from "./twin"
uses M from "./middle"
uses W from "./wrapper"
uses V from "./other/wrapper"
orbital Consumer {
  entity Item [runtime] {
    id : string!
    text : string = ""
  }
  trait Reading = A.traits.Cycle -> Item { events { NEXT: ADVANCE } }
  trait TwinReading = B.traits.Cycle -> Item {}
}
orbital Imported = A.orbitals.Mechanism {}
orbital Nested = M.orbitals.Middle { traits { Named { events { NEXT: RUN } } } }
orbital CopyOne = W.orbitals.Inner {}
orbital CopyTwo = V.orbitals.Inner {}
"#;
        let reply = inspect(
            source,
            &json!({"path":dir.path().join("consumer.lolo"),"version":2}),
        )
        .unwrap();
        let anchors = reply["sourceMap"].as_array().unwrap();
        for pointer in [
            "orbitals[0].traits[0].stateMachine.transitions[0]",
            "orbitals[1].traits[0].stateMachine.transitions[0]",
        ] {
            let arm = anchors.iter().find(|a| a["pointer"] == pointer).unwrap();
            assert_eq!(arm["path"], atom.to_string_lossy().as_ref());
            assert_eq!(arm["line"], 10);
            assert_eq!(arm["exact"], true);
            assert_ne!(
                arm["traitId"], arm["originTraitId"],
                "resolved consumer declaration and original atom have different identities"
            );
        }
        for (pointer, original) in [
            ("orbitals[3].traits[0].stateMachine.transitions[0]", &atom),
            (
                "orbitals[4].traits[0].stateMachine.transitions[0]",
                &other_atom,
            ),
        ] {
            let arm = anchors.iter().find(|a| a["pointer"] == pointer).unwrap();
            assert_eq!(
                arm["path"],
                original.to_string_lossy().as_ref(),
                "nested duplicate modules remain file-qualified"
            );
        }
        let declarations: Vec<&Value> = reply["program"]["orbitals"]
            .as_array()
            .unwrap()
            .iter()
            .map(|orbital| &orbital["traits"][0]["sourceDeclaration"])
            .collect();
        assert!(declarations
            .iter()
            .all(|source| source["fingerprint"].as_str().is_some()));
        assert!(declarations
            .iter()
            .all(|source| source.get("path").is_none()));
        assert_eq!(declarations[0], declarations[1]);
        assert_eq!(declarations[0], declarations[2]);
        let nested_trait_id = &reply["program"]["orbitals"][2]["traits"][0]["id"];
        let nested_binding = reply["composition"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| &c["traitId"] == nested_trait_id)
            .unwrap();
        assert_eq!(nested_binding["authoredPointer"], "orbitals[2].reference");
        assert_eq!(nested_binding["traitOverrideKey"], "Named");
        assert_eq!(nested_binding["source"]["exact"], true);
        assert!(
            nested_binding["source"]["scopeLength"].as_u64().unwrap()
                > nested_binding["source"]["length"].as_u64().unwrap()
        );
        assert_eq!(
            nested_binding["binding"]["traits"]["Named"]["events"]["NEXT"],
            "RUN"
        );
        let nested_arm = anchors
            .iter()
            .find(|a| a["pointer"] == "orbitals[2].traits[0].stateMachine.transitions[0]")
            .unwrap();
        assert_eq!(nested_arm["path"], atom.to_string_lossy().as_ref());
        let twin_arm = anchors
            .iter()
            .find(|a| a["pointer"] == "orbitals[0].traits[1].stateMachine.transitions[0]")
            .unwrap();
        assert_eq!(twin_arm["path"], twin.to_string_lossy().as_ref(), "identical trait IDs from different files are disambiguated by native source provenance");
        assert_eq!(
            reply["program"]["orbitals"][0]["traits"][0]["stateMachine"]["transitions"][0]["event"],
            "ADVANCE"
        );
        assert_eq!(
            reply["authoredProgram"]["orbitals"][0]["traits"][0]["events"]["NEXT"],
            "ADVANCE"
        );
        let imported = reply["imports"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["path"] == atom.to_string_lossy().as_ref())
            .unwrap();
        assert_eq!(
            imported["authoredProgram"]["orbitals"][0]["traits"][0]["stateMachine"]["transitions"]
                [0]["event"],
            "NEXT"
        );
        assert!(!reply["imports"].as_array().unwrap().is_empty());
    }

    #[test]
    fn inspect_requires_original_absolute_path() {
        assert!(inspect(ATOM, &json!({}))
            .unwrap_err()
            .message
            .contains("original absolute"));
        assert!(inspect(ATOM, &json!({"path":"relative.lolo"}))
            .unwrap_err()
            .message
            .contains("original absolute"));
    }
}
