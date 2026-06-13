//! `orb trace <output>:<line>[:<col>]` — reverse lookup from a generated
//! file position back to its `.orb` origin via the sidecar Source Map v3.
//!
//! Reads `<output>.map`, parses as Source Map v3, and resolves
//! (line, col) → (source, source_line, source_col, …). Prints a
//! human-readable single-line summary.
//!
//! When Phase 6 (`.lolo` → `.orb` chained maps) ships, this subcommand
//! will also walk the `.orb.map` sidecar to print the `.lolo` origin of
//! the same line. For now it stops at the `.orb` level.

use anyhow::{anyhow, Context, Result};
use std::fs;
use std::path::Path;

use sourcemap::SourceMap;

/// Parse a `<file>:<line>[:<col>]` position string.
///
/// Lines and columns are 1-indexed on the CLI (matching `grep`, `vim`,
/// compiler errors) but Source Map v3 uses 0-indexed internally — we do
/// the translation at the boundary.
pub fn parse_pos(arg: &str) -> Result<(String, u32, u32)> {
    let (path, coord) = arg
        .rsplit_once(':')
        .map(|(a, b)| match b.parse::<u32>() {
            Ok(col_or_line) => {
                // We have one number so far. Split the path again to see
                // if there's a second number (file:line:col).
                if let Some((path2, line_str)) = a.rsplit_once(':') {
                    if let Ok(line) = line_str.parse::<u32>() {
                        return (path2.to_string(), Some((line, col_or_line)));
                    }
                }
                (a.to_string(), Some((col_or_line, 1)))
            }
            Err(_) => (arg.to_string(), None),
        })
        .ok_or_else(|| anyhow!("expected <file>:<line>[:<col>]"))?;
    let (line, col) =
        coord.ok_or_else(|| anyhow!("expected <file>:<line>[:<col>], got {:?}", arg))?;
    if line == 0 {
        return Err(anyhow!("line must be ≥ 1"));
    }
    Ok((path, line, col))
}

/// Entry point invoked from `main.rs` for `orb trace`.
pub fn run(position: &str) -> Result<()> {
    let (output_path, cli_line, cli_col) = parse_pos(position)?;

    // Sidecar .map beside the output file.
    let output_path = Path::new(&output_path);
    let map_path = super::sourcemap::map_path_for_output(output_path);
    if !map_path.exists() {
        return Err(anyhow!(
            "no source map found at {} — did you run `orb emit … --map`?",
            map_path.display()
        ));
    }

    let contents =
        fs::read_to_string(&map_path).with_context(|| format!("reading {}", map_path.display()))?;
    let sm = SourceMap::from_reader(contents.as_bytes())
        .with_context(|| format!("parsing {} as Source Map v3", map_path.display()))?;

    // CLI is 1-indexed; Source Map v3 is 0-indexed. Column in the .map is
    // 0-indexed too.
    let gen_line = cli_line.saturating_sub(1);
    let gen_col = cli_col.saturating_sub(1);

    let Some(tok) = sm.lookup_token(gen_line, gen_col) else {
        return Err(anyhow!(
            "no mapping at {}:{}:{} (map has {} tokens)",
            output_path.display(),
            cli_line,
            cli_col,
            sm.get_token_count()
        ));
    };

    let src = tok.get_source().unwrap_or("<unknown>").to_string();
    let src_line = tok.get_src_line() + 1; // → 1-indexed for user output
    let src_col = tok.get_src_col() + 1;

    println!(
        "{}:{}:{}  →  {}:{}:{}",
        output_path.display(),
        cli_line,
        cli_col,
        src,
        src_line,
        src_col
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pos_file_line_col() {
        let (p, l, c) = parse_pos("out.ts:12:4").unwrap();
        assert_eq!(p, "out.ts");
        assert_eq!(l, 12);
        assert_eq!(c, 4);
    }

    #[test]
    fn parse_pos_file_line_only() {
        let (p, l, c) = parse_pos("out.ts:12").unwrap();
        assert_eq!(p, "out.ts");
        assert_eq!(l, 12);
        assert_eq!(c, 1); // default column
    }

    #[test]
    fn parse_pos_rejects_zero_line() {
        assert!(parse_pos("out.ts:0").is_err());
    }
}
