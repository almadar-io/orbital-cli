//! S-expression evaluation command + JSON-to-Value conversion.

/// Evaluate an S-expression and print the result.
///
/// The expression is parsed from JSON, evaluated in a fresh context (with optional
/// @entity and @payload bindings), and the result is printed according to `output_format`:
/// - "value"  — human-readable via Display (default)
/// - "json"   — compact JSON
/// - "pretty" — indented JSON
pub(crate) fn evaluate_sexpr(
    expr_src: Option<&str>,
    entity_json: Option<&str>,
    payload_json: Option<&str>,
    output_format: &str,
) -> anyhow::Result<()> {
    use std::io::Read;

    // Read source: argument or stdin
    let src = if let Some(s) = expr_src {
        s.to_string()
    } else {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        buf
    };

    let src = src.trim();
    if src.is_empty() {
        anyhow::bail!("No S-expression provided. Pass it as an argument or via stdin.");
    }

    let entity = entity_json
        .map(|json| {
            serde_json::from_str(json)
                .map_err(|e| anyhow::anyhow!("--entity JSON parse error: {}", e))
        })
        .transpose()?;
    let payload = payload_json
        .map(|json| {
            serde_json::from_str(json)
                .map_err(|e| anyhow::anyhow!("--payload JSON parse error: {}", e))
        })
        .transpose()?;
    let expr: serde_json::Value =
        serde_json::from_str(src).map_err(|e| anyhow::anyhow!("JSON parse error: {}", e))?;
    let result = evaluate_value(&expr, entity.as_ref(), payload.as_ref())?;

    // Output
    match output_format {
        "json" => {
            println!("{}", serde_json::to_string(&result)?);
        }
        "pretty" => {
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        _ => {
            // "value": human-readable — strings without quotes, numbers as integers when whole
            println!("{}", result.to_string_value());
        }
    }

    Ok(())
}

/// Evaluate an S-expression with optional `@entity` / `@payload` bindings — what
/// `orb evaluate` prints and `orb mcp` returns.
pub(crate) fn evaluate_value(
    expr: &serde_json::Value,
    entity: Option<&serde_json::Value>,
    payload: Option<&serde_json::Value>,
) -> anyhow::Result<orbital_core::Value> {
    use orbital_core::{BindingContext, Evaluator, SExpression};

    let expr: SExpression = serde_json::from_value(expr.clone())
        .map_err(|e| anyhow::anyhow!("JSON parse error: {}", e))?;

    // Build context. The viewer comes from ALMADAR_PERSONA like every other
    // headless surface, so `@user.role` can be probed from the CLI instead of
    // silently evaluating against Null. This probe holds no schema, so only the
    // self-contained JSON persona form resolves — a bare id/role would need an
    // app's declared [identity] roster and is rejected with that explanation.
    let mut ctx = BindingContext::new();
    ctx.set_user(match orbital_core::runtime::persona_from_env_spec()
        .map_err(|e| anyhow::anyhow!("{}", e))?
    {
        orbital_core::runtime::EnvPersona::Resolved(user) => user,
        orbital_core::runtime::EnvPersona::None => orbital_core::Value::Null,
        orbital_core::runtime::EnvPersona::Bare(spec) => {
            return Err(anyhow::anyhow!(
                "Persona \"{spec}\" is a bare id/role, which resolves against an app's declared [identity] roster — this schema-less probe accepts only the JSON persona form (e.g. {{\"id\":\"member-1\",\"role\":\"member\"}})."
            ));
        }
    });

    if let Some(raw) = entity {
        ctx.set_entity(json_to_value(raw));
    }
    if let Some(raw) = payload {
        ctx.set_payload(json_to_value(raw));
    }

    // Evaluate
    let evaluator = Evaluator::new();
    evaluator
        .evaluate(&expr, &ctx)
        .map_err(|e| anyhow::anyhow!("Evaluation error: {}", e))
}

/// Convert serde_json::Value to orbital_core::Value
pub(crate) fn json_to_value(json: &serde_json::Value) -> orbital_core::Value {
    use orbital_core::Value;

    match json {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => Value::Number(n.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(s) => Value::String(s.clone()),
        serde_json::Value::Array(arr) => Value::Array(arr.iter().map(json_to_value).collect()),
        serde_json::Value::Object(obj) => Value::Object(
            obj.iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect(),
        ),
    }
}
