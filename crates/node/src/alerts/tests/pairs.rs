//! Every alert the node resolves is resolved under the key it is raised with.
//!
//! The router keys an episode by every field but the message, so a resolve whose fields differ from its
//! raise's never ends the episode. The check reads the node's own sources: each `alert =` and
//! `alert_resolved =` log site gives its field names, and the router must take each resolve of an alert as ending
//! an episode that one of its raises opens, and each raise as ended by one of its resolves. A field's value is not
//! read: its text differs by scope, so a resolve that carries a name with another value is for a test of its own.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::alerts::resolution::ends_episode;
use crate::test_support::Alert;

/// A log site's sorted field names.
type Identity = Vec<String>;

#[derive(Default)]
struct Sites {
    raised: BTreeMap<Identity, Vec<String>>,
    resolved: BTreeMap<Identity, Vec<String>>,
}

#[test]
fn every_alert_is_resolved_under_the_key_it_is_raised_with() {
    let mut alerts: BTreeMap<String, Sites> = BTreeMap::new();
    collect(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut alerts);
    assert!(alerts.len() > 40, "the scan found only {} alerts", alerts.len());

    let mut disagreements = Vec::new();
    for (name, sites) in alerts.iter().filter(|(_, sites)| !sites.resolved.is_empty() && !sites.raised.is_empty()) {
        let ends = |raised: &Identity, resolved: &Identity| {
            ends_episode(&captured("alert", name, raised), &captured("alert_resolved", name, resolved))
        };
        let unended = sites.raised.keys().any(|raised| !sites.resolved.keys().any(|resolved| ends(raised, resolved)));
        let unopened = sites.resolved.keys().any(|resolved| !sites.raised.keys().any(|raised| ends(raised, resolved)));
        if unended || unopened {
            let mut report = format!("{name}:");
            for (verb, group) in [("raised", &sites.raised), ("resolved", &sites.resolved)] {
                for (identity, at) in group {
                    report += &format!("\n    {verb} with {identity:?} at {}", at.join(", "));
                }
            }
            disagreements.push(report);
        }
    }
    assert!(disagreements.is_empty(), "alerts raised and resolved under different keys:\n{}", disagreements.join("\n"));
}

#[test]
fn a_site_reads_its_field_names_and_stops_at_the_message() {
    let site = fields_of(r#"alert = "x", key = %key, ?other, plain, "the {key} (here), and {}", skipped = 1"#);
    assert_eq!(site, ["key", "other", "plain"]);
    assert_eq!(fields_of(r#"alert = "x", key = %key"#), fields_of(r#"alert = "x", %key"#));
}

#[test]
fn a_resolve_with_other_fields_than_its_raise_does_not_end_its_episode() {
    let fields = vec!["a".to_string(), "b".to_string()];
    let raised = captured("alert", "x", &fields);
    assert!(ends_episode(&raised, &captured("alert_resolved", "x", &fields)));
    assert!(!ends_episode(&raised, &captured("alert_resolved", "x", &fields[..1].to_vec())));
}

/// The alert as a test would capture it: `name` under `kind` and each field with a value alike for both.
fn captured(kind: &str, name: &str, fields: &Identity) -> Alert {
    let mut alert: Alert = fields.iter().map(|field| (field.clone(), "x".to_string())).collect();
    alert.insert(kind.to_string(), name.to_string());
    alert
}

fn collect(dir: &Path, alerts: &mut BTreeMap<String, Sites>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if path.is_dir() {
            if !name.contains("test") {
                collect(&path, alerts);
            }
        } else if name.ends_with(".rs") && !name.contains("test") && !name.contains("scenario") {
            let source = fs::read_to_string(&path).unwrap();
            scan(&path.display().to_string(), without_test_module(&source), alerts);
        }
    }
}

/// `source` up to its inline test module, `#[cfg(test)]` over a `mod` that opens a block.
fn without_test_module(source: &str) -> &str {
    let mut offset = 0;
    let mut lines = source.split_inclusive('\n').peekable();
    while let Some(line) = lines.next() {
        if line.trim() == "#[cfg(test)]"
            && lines.peek().is_some_and(|next| next.starts_with("mod ") && next.trim_end().ends_with('{'))
        {
            return &source[..offset];
        }
        offset += line.len();
    }
    source
}

/// The alert sites of `source`, whose macro arguments are balanced parentheses over strings.
fn scan(file: &str, source: &str, alerts: &mut BTreeMap<String, Sites>) {
    let bytes = source.as_bytes();
    let mut from = 0;
    while let Some(found) = ["warn!(", "error!(", "info!(", "debug!("]
        .iter()
        .filter_map(|m| source[from..].find(m).map(|at| (at, m.len())))
        .min()
    {
        let start = from + found.0 + found.1;
        from = start;
        let Some(end) = closing(bytes, start) else { continue };
        let arguments = &source[start..end];
        let (Some(name), resolved) = alert_name(arguments) else { continue };
        let line = source[..start].matches('\n').count() + 1;
        let sites = alerts.entry(name).or_default();
        let group = if resolved { &mut sites.resolved } else { &mut sites.raised };
        group.entry(fields_of(arguments)).or_default().push(format!("{file}:{line}"));
    }
}

/// The end of the call whose arguments start at `start`.
fn closing(bytes: &[u8], start: usize) -> Option<usize> {
    let (mut depth, mut at) = (1, start);
    while at < bytes.len() {
        match bytes[at] {
            b'"' => at = string_end(bytes, at),
            b'\'' if bytes.get(at + 2) == Some(&b'\'') => at += 2,
            b'\'' if bytes.get(at + 1) == Some(&b'\\') && bytes.get(at + 3) == Some(&b'\'') => at += 3,
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
        at += 1;
    }
    None
}

/// The index of the quote closing the string that opens at `open`.
fn string_end(bytes: &[u8], open: usize) -> usize {
    let mut at = open + 1;
    while at < bytes.len() && bytes[at] != b'"' {
        at += if bytes[at] == b'\\' { 2 } else { 1 };
    }
    at
}

/// The comma-separated top-level parts of a macro's arguments.
fn parts(arguments: &str) -> Vec<&str> {
    let bytes = arguments.as_bytes();
    let (mut parts, mut depth, mut at, mut part_start) = (Vec::new(), 0, 0, 0);
    while at < bytes.len() {
        match bytes[at] {
            b'"' => at = string_end(bytes, at),
            b'\'' if bytes.get(at + 2) == Some(&b'\'') => at += 2,
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                parts.push(arguments[part_start..at].trim());
                part_start = at + 1;
            }
            _ => {}
        }
        at += 1;
    }
    parts.push(arguments[part_start..].trim());
    parts.retain(|part| !part.is_empty());
    parts
}

/// The alert a site names, and whether it resolves it.
fn alert_name(arguments: &str) -> (Option<String>, bool) {
    for part in parts(arguments) {
        for (field, resolved) in [("alert", false), ("alert_resolved", true)] {
            let value = part.strip_prefix(field).map(str::trim_start).and_then(|rest| rest.strip_prefix('='));
            if let Some(value) = value.map(str::trim).and_then(|value| value.strip_prefix('"')) {
                return (value.strip_suffix('"').map(str::to_string), resolved);
            }
        }
    }
    (None, false)
}

/// A site's field names but the alert's, up to its message.
fn fields_of(arguments: &str) -> Identity {
    let mut fields = Vec::new();
    for part in parts(arguments) {
        if part.starts_with('"') {
            break;
        }
        let name_end = part.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.' || c == '%' || c == '?'));
        let name = part[..name_end.unwrap_or(part.len())].trim_start_matches(['%', '?']);
        if !["alert", "alert_resolved"].contains(&name) {
            fields.push(name.to_string());
        }
    }
    fields.sort();
    fields
}
