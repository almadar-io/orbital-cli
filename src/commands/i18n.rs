//! `orb i18n` — inspect the baked vocabulary tables and check translation
//! coverage. Both subcommands read the same tables the parser/translator
//! use at runtime (`orbital_core::i18n`, `orbital_lolo::keywords`) — no
//! separate copy.

use orbital_core::i18n::{coverage::check_translation_coverage, tables};
use serde::Serialize;

#[derive(Serialize)]
struct VocabOutput<'a> {
    keywords: &'a [&'static str],
    shapes: &'a [&'static str],
    tags: &'a [&'static str],
    categories: &'a [&'static str],
    capabilities: &'a [&'static str],
    annotations: &'a [&'static str],
    sigils: &'a [&'static str],
    effects: &'a [&'static str],
    types: &'a [&'static str],
    units: &'a [&'static str],
    literals: &'a [&'static str],
    #[serde(rename = "reservedEvents")]
    reserved_events: &'a [&'static str],
    orb: Vec<&'static str>,
    operators: Vec<&'static str>,
}

/// `orb i18n vocab [--json]` — print the `.lolo` parser vocabulary plus the
/// `.orb` structural key list and the operator table (both from the baked
/// `en` definition, since those two sections have no separate parser-side
/// owner).
pub(crate) fn run_i18n_vocab(json: bool) -> anyhow::Result<()> {
    let v = orbital_lolo::keywords::vocabulary();
    let en = &tables::definitions()["en"];
    let mut orb: Vec<&str> = en.orb.keys().map(String::as_str).collect();
    orb.sort_unstable();
    let mut operators: Vec<&str> = en.operators.keys().map(String::as_str).collect();
    operators.sort_unstable();

    if json {
        let out = VocabOutput {
            keywords: v.keywords,
            shapes: v.shapes,
            tags: v.tags,
            categories: v.categories,
            capabilities: v.capabilities,
            annotations: v.annotations,
            sigils: v.sigils,
            effects: v.effects,
            types: v.types,
            units: v.units,
            literals: v.literals,
            reserved_events: v.reserved_events,
            orb,
            operators,
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    let print_section = |name: &str, words: &[&str]| {
        println!("{name} ({}): {}", words.len(), words.join(" "));
    };
    print_section("keywords", v.keywords);
    print_section("shapes", v.shapes);
    print_section("tags", v.tags);
    print_section("categories", v.categories);
    print_section("capabilities", v.capabilities);
    print_section("annotations", v.annotations);
    print_section("sigils", v.sigils);
    print_section("effects", v.effects);
    print_section("types", v.types);
    print_section("units", v.units);
    print_section("literals", v.literals);
    print_section("reservedEvents", v.reserved_events);
    print_section("orb", &orb);
    print_section("operators", &operators);
    Ok(())
}

/// `orb i18n check` — run the tables-vs-tables coverage gate
/// (`check_translation_coverage`, B1 rules 1-2: every non-English language
/// covers every `en` key, no empty values) plus the parser-vs-tables
/// equality gate (`orbital_lolo::keywords::vocabulary_vs_en_problems`).
/// Exits 1 and lists every problem (never truncated) when any are found.
pub(crate) fn run_i18n_check() -> anyhow::Result<()> {
    let coverage_problems = check_translation_coverage(tables::definitions());
    let vocab_problems = orbital_lolo::keywords::vocabulary_vs_en_problems();
    let parser_coverage_problems = orbital_lolo::keywords::non_english_coverage_problems();

    let total = coverage_problems.len() + vocab_problems.len() + parser_coverage_problems.len();
    if total == 0 {
        println!("orb i18n check: ok — 0 problems");
        return Ok(());
    }

    println!("orb i18n check: {total} problem(s)");
    if !vocab_problems.is_empty() {
        println!(
            "\nparser vocabulary vs orbital_core::i18n en.json ({}):",
            vocab_problems.len()
        );
        for p in &vocab_problems {
            println!("  {p}");
        }
    }
    if !coverage_problems.is_empty() {
        println!(
            "\ntranslation coverage — non-English tables vs en.json ({}):",
            coverage_problems.len()
        );
        for p in &coverage_problems {
            println!("  {}.{}.{}: {:?}", p.lang, p.section, p.key, p.kind);
        }
    }
    if !parser_coverage_problems.is_empty() {
        println!(
            "\ntranslation coverage — non-English tables vs orbital-lolo parser vocabulary ({}):",
            parser_coverage_problems.len()
        );
        for p in &parser_coverage_problems {
            println!("  {p}");
        }
    }
    std::process::exit(1);
}
