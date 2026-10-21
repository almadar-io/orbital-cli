//! `orb test` — exhaustive state-machine test generation + headless execution.

use std::path::PathBuf;

/// Run exhaustive state machine tests
///
/// Uses the ExhaustiveTestGenerator from orbital-compiler to generate test cases.
/// Currently generates and displays test cases; future versions will execute against runtime.
pub(crate) fn run_tests(
    path: &PathBuf,
    orbital_filter: Option<String>,
    trait_filter: Option<String>,
    report: &str,
    execute: bool,
    verbose: bool,
) -> anyhow::Result<()> {
    use orbital_compiler::{compile_file, CompilerOptions, ExhaustiveTestGenerator};

    println!("Loading schema: {}", path.display());

    // Compile schema to OIR
    let options = CompilerOptions::from_env();
    let module =
        compile_file(path, &options).map_err(|e| anyhow::anyhow!("Compilation failed: {}", e))?;
    crate::commands::validate::lint_gate(path)?;

    if verbose {
        println!("Compiled module: {}", module.name);
        println!("Traits found: {}", module.traits.len());
    }

    // Generate exhaustive tests
    let generator = ExhaustiveTestGenerator::new(&module);
    let mut results = generator.generate_all_tests();

    // Apply filters
    if let Some(ref orbital) = orbital_filter {
        results.retain(|r| {
            r.trait_name
                .to_lowercase()
                .contains(&orbital.to_lowercase())
        });
    }
    if let Some(ref trait_name) = trait_filter {
        results.retain(|r| {
            r.trait_name
                .to_lowercase()
                .contains(&trait_name.to_lowercase())
        });
    }

    if results.is_empty() {
        println!("No traits found matching the filter criteria.");
        return Ok(());
    }

    // If --execute, run headless execution against the state machine
    if execute {
        return execute_tests_headless(&module, &mut results, report, verbose);
    }

    // Output results based on report format
    match report {
        "json" => {
            let json = serde_json::to_string_pretty(&results)?;
            println!("{}", json);
        }
        _ => {
            // Text format
            let mut total_tests = 0;
            for result in &results {
                println!("\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                println!("Trait: {}", result.trait_name);
                println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                println!("Generated {} test cases:", result.tests.len());
                total_tests += result.tests.len();

                // Group tests by category
                let mut by_category: std::collections::HashMap<_, Vec<_>> =
                    std::collections::HashMap::new();
                for test in &result.tests {
                    by_category.entry(test.category).or_default().push(test);
                }

                for (category, tests) in &by_category {
                    println!("\n  {} ({} tests):", category, tests.len());
                    for test in tests {
                        if verbose {
                            println!("    • {} ", test.name);
                            println!(
                                "      from: {} → event: {} → to: {}",
                                test.from_state, test.event, test.expected_state
                            );
                            if !test.setup_path.is_empty() {
                                let path_str: Vec<_> = test
                                    .setup_path
                                    .iter()
                                    .map(|s| format!("{}→{}", s.event, s.expected_state))
                                    .collect();
                                println!("      setup: {}", path_str.join(" "));
                            }
                        } else {
                            println!("    • {}", test.name);
                        }
                    }
                }
            }

            println!("\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
            println!(
                "Total: {} traits, {} test cases generated",
                results.len(),
                total_tests
            );
            println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
            println!("\nHint: Use --execute to run tests headlessly against the state machine.");
            println!("Use --report json to get test cases for external test runners.");
        }
    }

    Ok(())
}

/// One generated test case played headlessly.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase", rename_all_fields = "camelCase", tag = "status")]
pub(crate) enum TestOutcome {
    Passed {
        trait_name: String,
        test: String,
    },
    Failed {
        trait_name: String,
        test: String,
        expected: String,
        got: String,
    },
    Skipped {
        trait_name: String,
        test: String,
        reason: String,
    },
}

/// Play every generated test case against the OIR state machine: start at the
/// initial state, walk the setup path to the case's `from` state, fire its event and
/// compare the resulting state. Records each trait's pass/fail counts on `results`.
pub(crate) fn walk_tests(
    module: &orbital_core::ir::OirModule,
    results: &mut [orbital_compiler::TraitTestResult],
) -> Vec<TestOutcome> {
    use orbital_compiler::TestCategory;

    let mut outcomes = Vec::new();
    for result in results.iter_mut() {
        let Some(trait_def) = module.traits.iter().find(|t| t.name == result.trait_name) else {
            outcomes.push(TestOutcome::Skipped {
                trait_name: result.trait_name.clone(),
                test: String::new(),
                reason: "trait not found in the compiled module".to_string(),
            });
            continue;
        };

        let initial_state = trait_def
            .states
            .iter()
            .find(|s| s.is_initial)
            .map(|s| s.name.as_str())
            .or_else(|| trait_def.states.first().map(|s| s.name.as_str()))
            .unwrap_or("idle");

        let (mut trait_passed, mut trait_failed) = (0usize, 0usize);
        for test in &result.tests {
            let skip = |reason: String| TestOutcome::Skipped {
                trait_name: result.trait_name.clone(),
                test: test.name.clone(),
                reason,
            };
            // Journey tests encode compound events in a single string.
            if test.category == TestCategory::Journey {
                outcomes.push(skip("journey test".to_string()));
                continue;
            }

            let mut current_state = initial_state.to_string();
            let mut setup_ok = true;
            for step in &test.setup_path {
                match orbital_compiler::resolve_transition(
                    trait_def,
                    &current_state,
                    &step.event,
                    step.payload.as_ref(),
                    test.entity_data.as_ref(),
                ) {
                    Some(new_state) => current_state = new_state,
                    None => {
                        setup_ok = false;
                        break;
                    }
                }
            }
            if !setup_ok {
                outcomes.push(skip("setup path blocked".to_string()));
                continue;
            }
            if current_state != test.from_state {
                outcomes.push(skip(format!(
                    "setup reached '{}', expected '{}'",
                    current_state, test.from_state
                )));
                continue;
            }

            let after_state = orbital_compiler::resolve_transition(
                trait_def,
                &current_state,
                &test.event,
                test.payload.as_ref(),
                test.entity_data.as_ref(),
            )
            .unwrap_or_else(|| current_state.clone());

            if after_state == test.expected_state {
                trait_passed += 1;
                outcomes.push(TestOutcome::Passed {
                    trait_name: result.trait_name.clone(),
                    test: test.name.clone(),
                });
            } else {
                trait_failed += 1;
                outcomes.push(TestOutcome::Failed {
                    trait_name: result.trait_name.clone(),
                    test: test.name.clone(),
                    expected: test.expected_state.clone(),
                    got: after_state,
                });
            }
        }
        result.passed = trait_passed;
        result.failed = trait_failed;
    }
    outcomes
}

/// `orb test --execute` as a value: every generated case of every trait, played.
pub(crate) fn exhaustive_test_report(path: &std::path::Path) -> anyhow::Result<serde_json::Value> {
    use orbital_compiler::{compile_file, CompilerOptions, ExhaustiveTestGenerator};

    let module = compile_file(path, &CompilerOptions::from_env())
        .map_err(|e| anyhow::anyhow!("Compilation failed: {}", e))?;
    crate::commands::validate::lint_gate(path)?;
    let mut results = ExhaustiveTestGenerator::new(&module).generate_all_tests();
    let outcomes = walk_tests(&module, &mut results);
    let count = |status: &str| {
        outcomes
            .iter()
            .filter(|o| {
                matches!(
                    (o, status),
                    (TestOutcome::Passed { .. }, "passed")
                        | (TestOutcome::Failed { .. }, "failed")
                        | (TestOutcome::Skipped { .. }, "skipped")
                )
            })
            .count()
    };
    Ok(serde_json::json!({
        "passed": count("passed"),
        "failed": count("failed"),
        "skipped": count("skipped"),
        "outcomes": outcomes,
    }))
}

/// Execute generated test cases headlessly against the OIR state machine.
fn execute_tests_headless(
    module: &orbital_core::ir::OirModule,
    results: &mut [orbital_compiler::TraitTestResult],
    report: &str,
    verbose: bool,
) -> anyhow::Result<()> {
    let outcomes = walk_tests(module, results);
    let (mut total_passed, mut total_failed, mut total_skipped) = (0usize, 0usize, 0usize);
    for outcome in &outcomes {
        match outcome {
            TestOutcome::Passed { test, .. } => {
                total_passed += 1;
                if verbose {
                    println!("  ✓ {test}");
                }
            }
            TestOutcome::Failed {
                test,
                expected,
                got,
                ..
            } => {
                total_failed += 1;
                println!("  ✗ {test} — expected '{expected}', got '{got}'");
            }
            TestOutcome::Skipped {
                trait_name,
                test,
                reason,
            } => {
                if test.is_empty() {
                    if verbose {
                        println!("⚠ Trait '{trait_name}' not found in module, skipping");
                    }
                    continue;
                }
                total_skipped += 1;
                if verbose {
                    println!("  ⊘ {test} ({reason})");
                }
            }
        }
    }

    match report {
        "json" => {
            let json = serde_json::to_string_pretty(&results)?;
            println!("{}", json);
        }
        _ => {
            println!("\n{}", "=".repeat(60));
            println!("Headless Execution Results");
            println!("{}", "=".repeat(60));

            for result in results.iter() {
                let status = if result.failed == 0 { "✓" } else { "✗" };
                println!(
                    "  {} {} — {}/{} passed{}",
                    status,
                    result.trait_name,
                    result.passed,
                    result.passed + result.failed,
                    if result.failed > 0 {
                        format!(" ({} failed)", result.failed)
                    } else {
                        String::new()
                    }
                );
            }

            println!("{}", "-".repeat(60));
            println!(
                "Total: {} passed, {} failed, {} skipped",
                total_passed, total_failed, total_skipped
            );
            println!("{}", "=".repeat(60));

            if total_failed > 0 {
                std::process::exit(1);
            }
        }
    }

    Ok(())
}
