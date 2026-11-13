//! The `# ikit:` header that declares what a script needs.
//!
//! ```sh
//! #!/usr/bin/env bash
//! # ikit:
//! #   name: create-db
//! #   runs_on: target
//! #   params:
//! #     PGCONTAINER: service.postgres.container
//! #     PGSUPERPASS: service.postgres.env.POSTGRES_PASSWORD
//! #     PGSUPERUSER: { from: service.postgres.env.POSTGRES_USER, default: postgres }
//! #   args: ["-y"]
//! ```
//!
//! The header starts at a `# ikit:` line among the file's leading comments and runs through
//! the comment lines after it whose text is indented (or empty), so a regular comment that
//! follows ends it. Each line loses its `#` and one space and the result is parsed as YAML.
//!
//! SQL scripts use `--` comments the same way:
//!
//! ```sql
//! -- ikit:
//! --   name: grant-readonly
//! --   runs_on: service:postgres
//! --   database: core
//! --   params:
//! --     RO_PASSWORD: service.core.env.RO_PASSWORD
//! CREATE ROLE readonly LOGIN PASSWORD :'RO_PASSWORD';
//! ```

use std::collections::BTreeMap;

use ikit_core::registry::Category;
use ikit_core::{CsvInput, Interpreter, ScriptParam, SourcePath};
use serde::Deserialize;
use serde_yaml_ng::Value;

/// A parsed header; every field is optional so CLI flags can fill or override it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Header {
    pub name: Option<String>,
    pub description: Option<String>,
    pub runs_on: Option<String>,
    pub interpreter: Option<Interpreter>,
    pub params: Vec<ScriptParam>,
    pub csv: Option<CsvInput>,
    pub args: Vec<String>,
    pub timeout: Option<u64>,
    pub database: Option<String>,
    pub transaction: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Doc {
    ikit: Option<RawHeader>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawHeader {
    name: Option<String>,
    description: Option<String>,
    runs_on: Option<String>,
    interpreter: Option<Interpreter>,
    #[serde(default)]
    params: BTreeMap<String, Value>,
    input: Option<RawInput>,
    #[serde(default)]
    args: Vec<String>,
    timeout: Option<u64>,
    database: Option<String>,
    transaction: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInput {
    csv: Option<RawCsv>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCsv {
    header: Option<String>,
    columns: Vec<String>,
    services: Option<Category>,
    arg: Option<bool>,
}

pub(crate) const DEFAULT_CSV_HEADER: &str = "db_name,db_user,db_password";

/// The header's YAML text, if the file has one: `# ikit:` or, for SQL, `-- ikit:`.
fn header_text(body: &str) -> Option<String> {
    let mut lines = body.lines().enumerate();
    let (start, prefix) = loop {
        let (i, line) = lines.next()?;
        let t = line.trim();
        match t {
            "# ikit:" | "#ikit:" => break (i, "#"),
            "-- ikit:" | "--ikit:" => break (i, "--"),
            _ => {}
        }
        // Only the leading comments are searched.
        if !t.is_empty() && !t.starts_with('#') && !t.starts_with("--") {
            return None;
        }
    };
    let mut yaml = vec!["ikit:".to_owned()];
    for line in body.lines().skip(start + 1) {
        let Some(text) = line.trim_start().strip_prefix(prefix) else {
            break;
        };
        let text = text.strip_prefix(' ').unwrap_or(text);
        if !text.is_empty() && !text.starts_with([' ', '\t']) {
            break;
        }
        yaml.push(text.to_owned());
    }
    Some(yaml.join("\n"))
}

/// Parses the file's `# ikit:` header; `None` when it has none.
pub(crate) fn parse(body: &str) -> Result<Option<Header>, String> {
    let Some(yaml) = header_text(body) else {
        return Ok(None);
    };
    let doc: Doc =
        serde_yaml_ng::from_str(&yaml).map_err(|e| format!("invalid `# ikit:` header: {e}"))?;
    let raw = doc.ikit.unwrap_or_default();
    let params = raw
        .params
        .into_iter()
        .map(|(name, v)| parse_param(&name, v).map_err(|e| format!("param {name}: {e}")))
        .collect::<Result<Vec<_>, _>>()?;
    let csv = raw.input.and_then(|i| i.csv).map(|c| CsvInput {
        header: c.header.unwrap_or_else(|| DEFAULT_CSV_HEADER.to_owned()),
        columns: c.columns,
        services: c.services,
        arg: c.arg.unwrap_or(true),
    });
    Ok(Some(Header {
        name: raw.name,
        description: raw.description,
        runs_on: raw.runs_on,
        interpreter: raw.interpreter,
        params,
        csv,
        args: raw.args,
        timeout: raw.timeout,
        database: raw.database,
        transaction: raw.transaction,
    }))
}

fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// `NAME: source`, or `NAME: { from | value, default, required, secret }`.
fn parse_param(name: &str, v: Value) -> Result<ScriptParam, String> {
    let mut param = ScriptParam {
        name: name.to_owned(),
        from: None,
        value: None,
        default: None,
        required: true,
        secret: false,
    };
    match v {
        Value::String(s) => param.from = Some(s.parse::<SourcePath>()?),
        Value::Mapping(map) => {
            for (k, v) in map {
                let key = k.as_str().unwrap_or_default();
                let text = || scalar(&v).ok_or_else(|| format!("`{key}` must be a plain value"));
                let flag = || {
                    v.as_bool()
                        .ok_or_else(|| format!("`{key}` must be true or false"))
                };
                match key {
                    "from" => param.from = Some(text()?.parse()?),
                    "value" => param.value = Some(text()?),
                    "default" => param.default = Some(text()?),
                    "required" => param.required = flag()?,
                    "secret" => param.secret = flag()?,
                    other => {
                        return Err(format!(
                            "unknown key `{other}` (expected from, value, default, required or secret)"
                        ));
                    }
                }
            }
        }
        _ => {
            return Err(
                "expected a source such as `service.postgres.env.POSTGRES_PASSWORD`, or { from: ..., default: ... } / { value: ... }"
                    .to_owned(),
            );
        }
    }
    match (&param.from, &param.value) {
        (Some(_), Some(_)) => Err("give `from` or `value`, not both".to_owned()),
        (None, None) => Err("give `from` or `value`".to_owned()),
        _ => Ok(param),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CREATE_DB: &str = r#"#!/usr/bin/env bash
# ikit:
#   name: create-db
#   description: Create roles and databases
#   runs_on: target
#   params:
#     PGCONTAINER: service.postgres.container
#     PGSUPERUSER: { from: service.postgres.env.POSTGRES_USER, default: postgres }
#     PGSUPERPASS: service.postgres.env.POSTGRES_PASSWORD
#     PORT: { value: 5432 }
#   input:
#     csv:
#       columns: [DB_NAME, DB_USER, DB_PASSWORD]
#       services: app
#   args: ["-y"]
#   timeout: 120
#
# Usage: create-db.sh [-y] file.csv
set -euo pipefail
"#;

    #[test]
    fn parses_a_full_header() {
        let h = parse(CREATE_DB).unwrap().unwrap();
        assert_eq!(h.name.as_deref(), Some("create-db"));
        assert_eq!(h.runs_on.as_deref(), Some("target"));
        assert_eq!(h.args, ["-y"]);
        assert_eq!(h.timeout, Some(120));
        let names: Vec<&str> = h.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["PGCONTAINER", "PGSUPERPASS", "PGSUPERUSER", "PORT"]);
        let user = &h.params[2];
        assert_eq!(
            user.from.as_ref().unwrap().to_string(),
            "service.postgres.env.POSTGRES_USER"
        );
        assert_eq!(user.default.as_deref(), Some("postgres"));
        assert_eq!(h.params[3].value.as_deref(), Some("5432"));
        let csv = h.csv.unwrap();
        assert_eq!(csv.header, DEFAULT_CSV_HEADER);
        assert_eq!(csv.services, Some(Category::App));
        assert!(csv.arg);
    }

    #[test]
    fn parses_sql_headers() {
        let body = "-- ikit:\n--   name: grants\n--   runs_on: service:postgres\n--   database: core\n--   transaction: true\n--   params:\n--     PW: service.core.env.DB_PASSWORD\n-- Grants for the read-only role.\nGRANT SELECT ON ALL TABLES IN SCHEMA public TO ro;\n";
        let h = parse(body).unwrap().unwrap();
        assert_eq!(h.name.as_deref(), Some("grants"));
        assert_eq!(h.database.as_deref(), Some("core"));
        assert_eq!(h.transaction, Some(true));
        assert_eq!(h.params[0].name, "PW");
        // A `#` line does not continue a `--` header.
        let mixed = "-- ikit:\n--   name: a\n#   runs_on: target\n";
        assert_eq!(parse(mixed).unwrap().unwrap().runs_on, None);
    }

    #[test]
    fn files_without_a_header_have_none() {
        assert_eq!(parse("#!/bin/sh\necho hi\n").unwrap(), None);
        // A header after the first command is not looked for.
        assert_eq!(parse("echo hi\n# ikit:\n#   name: x\n").unwrap(), None);
        assert_eq!(parse("# ikit:\necho\n").unwrap(), Some(Header::default()));
    }

    #[test]
    fn rejects_bad_headers() {
        for (body, needle) in [
            ("# ikit:\n#   nmae: x\n", "unknown field `nmae`"),
            (
                "# ikit:\n#   params:\n#     X: target.password\n",
                "param X",
            ),
            (
                "# ikit:\n#   params:\n#     X: { from: org, value: a }\n",
                "not both",
            ),
            (
                "# ikit:\n#   params:\n#     X: { dflt: a }\n",
                "unknown key",
            ),
            ("# ikit:\n#   params:\n#     X: 5\n", "expected a source"),
        ] {
            let e = parse(body).unwrap_err();
            assert!(e.contains(needle), "{body}: {e}");
        }
    }
}
