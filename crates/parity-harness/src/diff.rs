//! Diff utilities for parity reports (design D6).
//!
//! Two comparators, both returning human-readable difference entries and an
//! empty list when the inputs are equal: [`json_diff`] for structured MCP
//! payloads (`tools/list` results, tool-call responses) and [`text_diff`] for
//! CLI `--help`/usage output and effective-config dumps.

use std::collections::BTreeSet;

use serde_json::Value;

/// Structurally compare two JSON values; empty result means equal.
///
/// Differences are reported as one line per diverging path, e.g.
/// `$.tools[0].name: expected "search", got "lookup"`. Object keys are walked
/// in sorted order and arrays element-wise (a length mismatch is its own entry),
/// so output is deterministic for a given input pair.
pub fn json_diff(expected: &Value, actual: &Value) -> Vec<String> {
    let mut out = Vec::new();
    diff_value(expected, actual, "$", &mut out);
    out
}

fn diff_value(expected: &Value, actual: &Value, path: &str, out: &mut Vec<String>) {
    match (expected, actual) {
        (Value::Object(exp), Value::Object(act)) => {
            let keys = exp.keys().chain(act.keys()).collect::<BTreeSet<_>>();
            for key in keys {
                let child = format!("{path}.{key}");
                match (exp.get(key), act.get(key)) {
                    (Some(ev), Some(av)) => diff_value(ev, av, &child, out),
                    (Some(ev), None) => {
                        out.push(format!("{child}: expected {}, got <missing>", ev));
                    }
                    (None, Some(av)) => {
                        out.push(format!("{child}: expected <missing>, got {av}"));
                    }
                    (None, None) => {}
                }
            }
        }
        (Value::Array(exp), Value::Array(act)) => {
            if exp.len() != act.len() {
                out.push(format!(
                    "{path}[len]: expected {}, got {}",
                    exp.len(),
                    act.len()
                ));
            }
            for (i, (ev, av)) in exp.iter().zip(act.iter()).enumerate() {
                diff_value(ev, av, &format!("{path}[{i}]"), out);
            }
        }
        _ if expected != actual => {
            out.push(format!("{path}: expected {expected}, got {actual}"));
        }
        _ => {}
    }
}

/// Line-based diff of two texts (LCS alignment); empty result means equal.
///
/// Entries point at the lines that differ, e.g. `-3: only in expected: foo` /
/// `+4: only in actual: bar`. Output is canonical for a given input pair: all
/// deletions first (ascending source line), then all insertions — so reports
/// stay stable even though LCS alignment paths are not unique on ties.
/// Intended for small CLI outputs; complexity is O(lines_expected * lines_actual).
pub fn text_diff(expected: &str, actual: &str) -> Vec<String> {
    let exp_lines: Vec<&str> = expected.lines().collect();
    let act_lines: Vec<&str> = actual.lines().collect();

    // Classic LCS dynamic programming table over line indices.
    let (n, m) = (exp_lines.len(), act_lines.len());
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (1..=n).rev() {
        for j in (1..=m).rev() {
            dp[i][j] = if exp_lines[i - 1] == act_lines[j - 1] {
                dp[i - 1][j - 1] + 1
            } else {
                dp[i - 1][j].max(dp[i][j - 1])
            };
        }
    }

    // Backtrack: lines skipped on either side are the differences. Entries are
    // collected as (position, kind, line) and rendered in canonical order below;
    // the raw walk is bottom-up, so a plain reverse would not be enough — the
    // relative order of -/+ entries at neighbouring positions depends on the
    // alignment path taken through ties.
    #[derive(PartialEq, Eq, PartialOrd, Ord)]
    struct DiffEntry {
        /// 0 for deletions (expected side), 1 for insertions (actual side).
        kind: u8,
        position: usize,
        line: String,
    }

    let mut entries = Vec::new();
    let (mut i, mut j) = (n, m);
    while i > 0 && j > 0 {
        if exp_lines[i - 1] == act_lines[j - 1] {
            i -= 1;
            j -= 1;
        } else if dp[i - 1][j] >= dp[i][j - 1] {
            entries.push(DiffEntry {
                kind: 0,
                position: i,
                line: exp_lines[i - 1].to_owned(),
            });
            i -= 1;
        } else {
            entries.push(DiffEntry {
                kind: 1,
                position: j,
                line: act_lines[j - 1].to_owned(),
            });
            j -= 1;
        }
    }
    while i > 0 {
        entries.push(DiffEntry {
            kind: 0,
            position: i,
            line: exp_lines[i - 1].to_owned(),
        });
        i -= 1;
    }
    while j > 0 {
        entries.push(DiffEntry {
            kind: 1,
            position: j,
            line: act_lines[j - 1].to_owned(),
        });
        j -= 1;
    }

    // Deletions first (ascending source line), then insertions.
    entries.sort();
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        match entry.kind {
            0 => out.push(format!(
                "-{}: only in expected: {}",
                entry.position, entry.line
            )),
            _ => out.push(format!(
                "+{}: only in actual: {}",
                entry.position, entry.line
            )),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use serde_json::json;

    #[test]
    fn json_diff_equal_values_are_empty() {
        let value = json!({"a": [1, 2], "b": {"c": null}});
        assert!(json_diff(&value, &value).is_empty());
    }

    #[test]
    fn json_detects_changed_scalar_with_path() {
        let expected = json!({"tools": [{"name": "search"}]});
        let actual = json!({"tools": [{"name": "lookup"}]});
        assert_eq!(
            json_diff(&expected, &actual),
            vec!["$.tools[0].name: expected \"search\", got \"lookup\"".to_owned()]
        );
    }

    #[test]
    fn json_detects_missing_keys_on_both_sides() {
        let expected = json!({"a": 1, "gone": true});
        let actual = json!({"a": 1, "added": false});
        assert_eq!(
            json_diff(&expected, &actual),
            vec![
                "$.added: expected <missing>, got false",
                "$.gone: expected true, got <missing>",
            ]
        );
    }

    #[test]
    fn json_detects_array_length_and_element_mismatches() {
        let expected = json!([1, 2, 3]);
        let actual = json!([1, 9]);
        assert_eq!(
            json_diff(&expected, &actual),
            vec![
                "$[len]: expected 3, got 2".to_owned(),
                "$[1]: expected 2, got 9".to_owned(),
            ]
        );
    }

    #[test]
    fn text_diff_equal_texts_are_empty() {
        assert!(text_diff("a\nb\nc", "a\nb\nc").is_empty());
        assert!(text_diff("", "").is_empty());
    }

    #[test]
    fn text_diff_reports_changed_lines_with_positions() {
        let expected = "line1\nline2\nline3";
        let actual = "line1\nchanged\nline3\nextra";
        assert_eq!(
            text_diff(expected, actual),
            vec![
                "-2: only in expected: line2".to_owned(),
                "+2: only in actual: changed".to_owned(),
                "+4: only in actual: extra".to_owned(),
            ]
        );
    }

    #[test]
    fn text_diff_handles_one_sided_inputs() {
        assert_eq!(
            text_diff("only", ""),
            vec!["-1: only in expected: only".to_owned()]
        );
        assert_eq!(text_diff("", "added"), vec!["+1: only in actual: added"]);
    }
}
