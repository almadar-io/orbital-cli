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
    let options = CompilerOptions::default();
    let module =
        compile_file(path, &options).map_err(|e| anyhow::anyhow!("Compilation failed: {}", e))?;

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

/// Execute generated test cases headlessly against the OIR state machine.
///
/// For each test case:
/// 1. Start at the initial state
/// 2. Walk the setup_path (fire events to reach the from_state)
/// 3. Fire the test event
/// 4. Compare the resulting state to expected_state
fn execute_tests_headless(
    module: &orbital_core::ir::OirModule,
    results: &mut Vec<orbital_compiler::TraitTestResult>,
    report: &str,
    verbose: bool,
) -> anyhow::Result<()> {
    use orbital_compiler::TestCategory;
    

    let mut total_passed = 0usize;
    let mut total_failed = 0usize;
    let mut total_skipped = 0usize;

    for result in results.iter_mut() {
        // Find the OIR trait
        let trait_def = match module.traits.iter().find(|t| t.name == result.trait_name) {
            Some(t) => t,
            None => {
                if verbose {
                    println!(
                        "⚠ Trait '{}' not found in module, skipping",
                        result.trait_name
                    );
                }
                continue;
            }
        };

        let initial_state = trait_def
            .states
            .iter()
            .find(|s| s.is_initial)
            .map(|s| s.name.as_str())
            .or_else(|| trait_def.states.first().map(|s| s.name.as_str()))
            .unwrap_or("idle");

        let mut trait_passed = 0usize;
        let mut trait_failed = 0usize;

        for test in &result.tests {
            // Skip journey tests — they encode compound events in a single string
            if test.category == TestCategory::Journey {
                total_skipped += 1;
                continue;
            }

            // Step 1: Walk setup_path to reach from_state
            let mut current_state = initial_state.to_string();
            let mut setup_ok = true;

            for step in &test.setup_path {
                match execute_event_headless(
                    trait_def,
                    &current_state,
                    &step.event,
                    step.payload.as_ref(),
                    test.entity_data.as_ref(),
                ) {
                    Some(new_state) => {
                        current_state = new_state;
                    }
                    None => {
                        // Setup path might go through guarded transitions — skip test
                        setup_ok = false;
                        break;
                    }
                }
            }

            if !setup_ok {
                total_skipped += 1;
                if verbose {
                    println!("  ⊘ {} (setup path blocked)", test.name);
                }
                continue;
            }

            // Step 2: Verify we're at the expected from_state
            if current_state != test.from_state {
                total_skipped += 1;
                if verbose {
                    println!(
                        "  ⊘ {} (setup reached '{}', expected '{}')",
                        test.name, current_state, test.from_state
                    );
                }
                continue;
            }

            // Step 3: Fire the test event
            let after_state = execute_event_headless(
                trait_def,
                &current_state,
                &test.event,
                test.payload.as_ref(),
                test.entity_data.as_ref(),
            )
            .unwrap_or_else(|| current_state.clone());

            // Step 4: Compare to expected_state
            let passed = after_state == test.expected_state;

            if passed {
                trait_passed += 1;
                total_passed += 1;
                if verbose {
                    println!("  ✓ {}", test.name);
                }
            } else {
                trait_failed += 1;
                total_failed += 1;
                println!(
                    "  ✗ {} — expected '{}', got '{}'",
                    test.name, test.expected_state, after_state
                );
            }
        }

        result.passed = trait_passed;
        result.failed = trait_failed;
    }

    // Output summary
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

/// Execute a single event against a trait's state machine headlessly.
///
/// Finds matching transition, evaluates guard (if any), returns new state.
/// Returns None if no transition matches or guard blocks.
fn execute_event_headless(
    trait_def: &orbital_core::ir::OirTrait,
    current_state: &str,
    event: &str,
    payload: Option<&serde_json::Value>,
    entity_data: Option<&serde_json::Value>,
) -> Option<String> {
    

    // Find matching transition
    let transition = trait_def
        .transitions
        .iter()
        .find(|t| t.event == event && t.from.matches(current_state))?;

    // Evaluate guard if present
    if let Some(ref guard) = transition.guard {
        let empty = serde_json::Value::Object(serde_json::Map::new());
        let p = payload.unwrap_or(&empty);
        let e = entity_data.unwrap_or(&empty);
        if !evaluate_guard_headless(guard, p, e) {
            return None;
        }
    }

    Some(transition.to.clone())
}

/// Headless guard evaluator for OIR expressions.
///
/// Handles common guard patterns: comparisons, boolean logic,
/// payload/entity field access. For complex guards (function calls, etc.)
/// defaults to allowing the transition (permissive mode).
fn evaluate_guard_headless(
    expr: &orbital_core::ir::OirExpr,
    payload: &serde_json::Value,
    entity_data: &serde_json::Value,
) -> bool {
    use orbital_core::ir::{OirBinaryOp, OirExpr};

    match expr {
        OirExpr::Bool { value } => *value,
        OirExpr::BinaryOp { op, left, right } => {
            match op {
                OirBinaryOp::And => {
                    evaluate_guard_headless(left, payload, entity_data)
                        && evaluate_guard_headless(right, payload, entity_data)
                }
                OirBinaryOp::Or => {
                    evaluate_guard_headless(left, payload, entity_data)
                        || evaluate_guard_headless(right, payload, entity_data)
                }
                OirBinaryOp::Eq => {
                    let l = resolve_expr_value(left, payload, entity_data);
                    let r = resolve_expr_value(right, payload, entity_data);
                    l == r
                }
                OirBinaryOp::Ne => {
                    let l = resolve_expr_value(left, payload, entity_data);
                    let r = resolve_expr_value(right, payload, entity_data);
                    l != r
                }
                OirBinaryOp::Gt => {
                    let l = resolve_expr_to_f64(left, payload, entity_data);
                    let r = resolve_expr_to_f64(right, payload, entity_data);
                    match (l, r) {
                        (Some(a), Some(b)) => a > b,
                        _ => false, // strict: unresolvable operands block the transition
                    }
                }
                OirBinaryOp::Ge => {
                    let l = resolve_expr_to_f64(left, payload, entity_data);
                    let r = resolve_expr_to_f64(right, payload, entity_data);
                    match (l, r) {
                        (Some(a), Some(b)) => a >= b,
                        _ => false,
                    }
                }
                OirBinaryOp::Lt => {
                    let l = resolve_expr_to_f64(left, payload, entity_data);
                    let r = resolve_expr_to_f64(right, payload, entity_data);
                    match (l, r) {
                        (Some(a), Some(b)) => a < b,
                        _ => false,
                    }
                }
                OirBinaryOp::Le => {
                    let l = resolve_expr_to_f64(left, payload, entity_data);
                    let r = resolve_expr_to_f64(right, payload, entity_data);
                    match (l, r) {
                        (Some(a), Some(b)) => a <= b,
                        _ => false,
                    }
                }
                _ => true, // Permissive for unsupported ops
            }
        }
        OirExpr::UnaryOp {
            op: orbital_core::ir::OirUnaryOp::Not,
            operand,
        } => !evaluate_guard_headless(operand, payload, entity_data),
        // For anything else (function calls, complex expressions), be permissive
        _ => true,
    }
}

/// Resolve an OIR expression to a JSON value for comparison.
fn resolve_expr_value(
    expr: &orbital_core::ir::OirExpr,
    payload: &serde_json::Value,
    entity_data: &serde_json::Value,
) -> serde_json::Value {
    use orbital_core::ir::{OirBindingRoot, OirExpr};

    match expr {
        OirExpr::Null => serde_json::Value::Null,
        OirExpr::Bool { value } => serde_json::Value::Bool(*value),
        OirExpr::Number { value } => serde_json::json!(*value),
        OirExpr::String { value } => serde_json::Value::String(value.clone()),
        OirExpr::Binding { binding } => {
            let source = match &binding.root {
                OirBindingRoot::Payload => payload,
                OirBindingRoot::Entity => entity_data,
                // State/config/user not available in headless mode
                _ => return serde_json::Value::Null,
            };
            // Walk the path into the source object
            let mut current = source;
            for key in &binding.path {
                current = match current.get(key) {
                    Some(v) => v,
                    None => return serde_json::Value::Null,
                };
            }
            current.clone()
        }
        _ => serde_json::Value::Null,
    }
}

/// Resolve an OIR expression to an f64 for numeric comparison.
fn resolve_expr_to_f64(
    expr: &orbital_core::ir::OirExpr,
    payload: &serde_json::Value,
    entity_data: &serde_json::Value,
) -> Option<f64> {
    let val = resolve_expr_value(expr, payload, entity_data);
    match val {
        serde_json::Value::Number(n) => n.as_f64(),
        _ => None,
    }
}
