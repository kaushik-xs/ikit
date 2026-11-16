//! `template:` env values: literal text with `{{reference}}` placeholders, e.g.
//! `template:postgresql://{{env.DB_USER}}:{{env.DB_PASSWORD}}@{{service:postgres.host}}:5432/{{env.DB_NAME}}`.
//!
//! A placeholder is anything a `from:` reference may be (`service:…`, `resource:…`), the
//! service's own env (`env.<VAR>`), or `org`, `workload`, `env`, `service`.

/// The prefix that marks a `from:` value as a template.
pub const TEMPLATE_PREFIX: &str = "template:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part<'a> {
    Text(&'a str),
    Ref(&'a str),
}

/// The template body of a `from:` value, if it is one.
pub fn template_body(from: &str) -> Option<&str> {
    from.strip_prefix(TEMPLATE_PREFIX)
}

/// Splits a template into text and placeholders. Placeholders are trimmed.
pub fn parse_template(body: &str) -> Result<Vec<Part<'_>>, String> {
    let mut parts = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("{{") {
        if start > 0 {
            parts.push(Part::Text(&rest[..start]));
        }
        let after = &rest[start + 2..];
        let end = after
            .find("}}")
            .ok_or_else(|| format!("template `{body}` has an unclosed `{{{{`"))?;
        let reference = after[..end].trim();
        if reference.is_empty() {
            return Err(format!("template `{body}` has an empty `{{{{}}}}`"));
        }
        if reference.contains("{{") {
            return Err(format!("template `{body}` nests `{{{{`"));
        }
        parts.push(Part::Ref(reference));
        rest = &after[end + 2..];
    }
    if rest.contains("}}") {
        return Err(format!("template `{body}` has a `}}}}` without `{{{{`"));
    }
    if !rest.is_empty() {
        parts.push(Part::Text(rest));
    }
    Ok(parts)
}

/// The placeholders of a template, in order.
pub fn template_refs(body: &str) -> Result<Vec<&str>, String> {
    Ok(parse_template(body)?
        .into_iter()
        .filter_map(|p| match p {
            Part::Ref(r) => Some(r),
            Part::Text(_) => None,
        })
        .collect())
}

/// The env var a `env.<VAR>` placeholder reads.
pub fn own_env_ref(reference: &str) -> Option<&str> {
    reference.strip_prefix("env.")
}

/// Placeholders that need no service: `org`, `workload`, `env`, `service`.
pub const CONTEXT_REFS: &[&str] = &["org", "workload", "env", "service"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_text_and_placeholders() {
        let parts = parse_template("http://{{ service:api.host }}:{{service:api.port}}/x").unwrap();
        assert_eq!(
            parts,
            [
                Part::Text("http://"),
                Part::Ref("service:api.host"),
                Part::Text(":"),
                Part::Ref("service:api.port"),
                Part::Text("/x"),
            ]
        );
        assert_eq!(parse_template("plain").unwrap(), [Part::Text("plain")]);
    }

    #[test]
    fn rejects_broken_braces() {
        for bad in ["{{x", "x}}", "{{}}", "{{a{{b}}"] {
            assert!(parse_template(bad).is_err(), "{bad}");
        }
    }
}
