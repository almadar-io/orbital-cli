//! `orb reference` / `orb signatures`: the language catalog, generated from code —
//! constructs and vocabulary from `orbital-lolo`, validation codes from
//! `orbital-compiler`'s `ErrorCode`, operators and patterns from the installed
//! language data, examples from the installed `@almadar/std` behaviors.

use orbital_compiler::phases::validation::ErrorCode;
use orbital_lolo::ast::Program;
use serde_json::{json, Value};

const CODES_PREFIX: &str = "codes/";

pub(crate) fn index() -> Value {
    let constructs: Vec<Value> = orbital_lolo::reference::constructs()
        .iter()
        .map(|c| json!({ "name": c.name, "kind": c.kind }))
        .collect();
    let mut categories = std::collections::BTreeMap::<&str, usize>::new();
    for code in ErrorCode::ALL {
        *categories.entry(code.category()).or_default() += 1;
    }
    json!({
        "constructs": constructs,
        "vocabulary": vocabulary(),
        "codes": categories
            .into_iter()
            .map(|(category, count)| json!({ "topic": format!("{CODES_PREFIX}{category}"), "count": count }))
            .collect::<Vec<_>>(),
    })
}

pub(crate) fn vocabulary() -> Value {
    let v = orbital_lolo::keywords::vocabulary();
    json!({
        "keywords": v.keywords,
        "shapes": v.shapes,
        "tags": v.tags,
        "categories": v.categories,
        "capabilities": v.capabilities,
        "annotations": v.annotations,
        "sigils": v.sigils,
        "effects": v.effects,
        "types": v.types,
        "units": v.units,
        "literals": v.literals,
        "reservedEvents": v.reserved_events,
    })
}

fn codes(category: Option<&str>) -> Vec<Value> {
    ErrorCode::ALL
        .iter()
        .filter(|c| category.is_none_or(|cat| c.category() == cat))
        .map(|c| {
            json!({
                "code": c.as_str(),
                "category": c.category(),
                "description": c.description(),
            })
        })
        .collect()
}

/// One topic: a construct (with real examples from `programs`), `vocabulary`, or `codes/<category>`.
pub(crate) fn topic(name: &str, programs: &[Program]) -> anyhow::Result<Value> {
    if name == "vocabulary" {
        return Ok(vocabulary());
    }
    if let Some(category) = name.strip_prefix(CODES_PREFIX) {
        let found = codes(Some(category));
        if found.is_empty() {
            anyhow::bail!("no validation codes in category `{category}`");
        }
        return Ok(json!({ "category": category, "codes": found }));
    }
    let construct = orbital_lolo::reference::constructs()
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            anyhow::anyhow!("no reference topic `{name}`; `orb reference` lists them")
        })?;
    let all = orbital_lolo::reference::examples(construct.name, programs);
    // A typical occurrence (median length) and a fuller one (90th percentile).
    let examples: Vec<&String> = [all.get(all.len() / 2), all.get(all.len() * 9 / 10)]
        .into_iter()
        .flatten()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut value = serde_json::to_value(construct)?;
    value["examples"] = json!(examples);
    Ok(value)
}

/// Everything at once: what `orb signatures` prints.
pub(crate) fn everything() -> anyhow::Result<Value> {
    let operators: Value =
        serde_json::from_str(&orbital_core::language_data::operator_data()?.operators)?;
    let patterns: Value =
        serde_json::from_str(&orbital_core::language_data::language_data()?.patterns)?;
    Ok(json!({
        "constructs": orbital_lolo::reference::constructs(),
        "vocabulary": vocabulary(),
        "codes": codes(None),
        "operators": operators["operators"],
        "patterns": patterns["patterns"],
    }))
}

/// The installed `@almadar/std` behaviors as parsed programs, the source of the examples.
pub(crate) fn std_programs() -> anyhow::Result<Vec<Program>> {
    let packages = crate::commands::npm_project::discover_from(&std::env::current_dir()?)?;
    let Some(std_pkg) = packages.iter().find(|p| p.prefix == "almadar-std") else {
        return Ok(Vec::new());
    };
    let ext = std_pkg.registry.extension();
    let mut files = Vec::new();
    let mut stack = vec![std_pkg.registry.root().to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == ext) {
                files.push(path);
            }
        }
    }
    files.sort();
    let mut programs = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file)?;
        let program = if ext == "lolo" {
            orbital_lolo::parse_str(&text).ok()
        } else {
            serde_json::from_str::<orbital_core::schema::OrbitalSchema>(&text)
                .ok()
                .and_then(|schema| orbital_lolo::unlower(&schema).ok())
        };
        programs.extend(program);
    }
    Ok(programs)
}

pub(crate) fn run_reference(topic_name: Option<&str>) -> anyhow::Result<()> {
    let value = match topic_name {
        None => index(),
        Some(name) => topic(name, &std_programs()?)?,
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOOR: &str = "app Door [version: \"1.0.0\"]\norbital DoorOrbital {\n  entity Door [runtime] {\n    id : string!\n  }\n  trait DoorLife -> Door [interaction] {\n    initial: closed\n    state closed {\n      INIT -> closed\n      OPEN -> open\n    }\n    state open {\n      CLOSE -> closed\n    }\n  }\n  page \"/door\" -> DoorLife\n}\n";

    #[test]
    fn the_index_lists_constructs_vocabulary_and_code_topics() {
        let i = index();
        let names: Vec<&str> = i["constructs"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["name"].as_str())
            .collect();
        assert!(names.contains(&"Trait") && names.contains(&"Entity"));
        assert!(i["vocabulary"]["keywords"]
            .as_array()
            .unwrap()
            .contains(&json!("trait")));
        assert!(i["codes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["topic"] == "codes/circuit"));
    }

    #[test]
    fn a_construct_topic_carries_doc_fields_and_real_examples() {
        let programs = [orbital_lolo::parse_str(DOOR).unwrap()];
        let t = topic("trait", &programs).unwrap();
        assert_eq!(t["name"], "Trait");
        assert!(!t["doc"].as_str().unwrap().is_empty());
        assert!(t["fields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["name"] == "states"));
        assert!(t["examples"][0]
            .as_str()
            .unwrap()
            .starts_with("trait DoorLife"));
    }

    #[test]
    fn a_construct_with_no_example_has_an_empty_list() {
        let t = topic("Tick", &[]).unwrap();
        assert_eq!(t["examples"], json!([]));
    }

    #[test]
    fn a_codes_topic_lists_each_code_with_its_description() {
        let t = topic("codes/circuit", &[]).unwrap();
        let dead_end = t["codes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["code"] == "CIRCUIT_DEAD_END_STATE")
            .expect("CIRCUIT_DEAD_END_STATE");
        assert_eq!(dead_end["description"], "State has no outgoing transitions");
    }

    #[test]
    fn an_unknown_topic_or_category_is_an_error() {
        assert!(topic("NoSuchThing", &[]).is_err());
        assert!(topic("codes/no-such-category", &[]).is_err());
    }
}
