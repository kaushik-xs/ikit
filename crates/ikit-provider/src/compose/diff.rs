//! What `apply` would change: desired compose files vs the ones last applied on the target.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_yaml_ng::Value;

use super::render::Rendered;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum Change {
    Add {
        service: String,
        image: String,
    },
    Remove {
        service: String,
    },
    /// `details` are human-readable lines; secret values never appear in them.
    Update {
        service: String,
        details: Vec<String>,
    },
}

impl Change {
    pub fn service(&self) -> &str {
        match self {
            Change::Add { service, .. }
            | Change::Remove { service }
            | Change::Update { service, .. } => service,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Plan {
    /// Nothing has been applied to the target yet.
    pub first_apply: bool,
    pub changes: Vec<Change>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

fn parse(yaml: &str) -> Result<Value, serde_yaml_ng::Error> {
    if yaml.trim().is_empty() {
        Ok(Value::Null)
    } else {
        serde_yaml_ng::from_str(yaml)
    }
}

fn services(doc: &Value) -> BTreeMap<String, Value> {
    doc.get("services")
        .and_then(Value::as_mapping)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| Some((k.as_str()?.to_owned(), v.clone())))
                .collect()
        })
        .unwrap_or_default()
}

fn string_map(v: Option<&Value>) -> BTreeMap<String, String> {
    v.and_then(Value::as_mapping)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| {
                    let v = match v {
                        Value::String(s) => s.clone(),
                        other => serde_yaml_ng::to_string(other).ok()?.trim().to_owned(),
                    };
                    Some((k.as_str()?.to_owned(), v.replace("$$", "$")))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn compact(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "none".to_owned(),
        Some(v) => serde_json::to_string(v).unwrap_or_default(),
    }
}

/// Lines describing how `old` became `new` for one service. Secret env (`old_secret`,
/// `new_secret`) is compared but only reported by key.
fn service_details(
    old: &Value,
    new: &Value,
    old_secret: &Value,
    new_secret: &Value,
) -> Vec<String> {
    let mut out = Vec::new();
    if old.get("image") != new.get("image") {
        out.push(format!(
            "image {} → {}",
            compact(old.get("image")),
            compact(new.get("image"))
        ));
    }

    let (old_env, new_env) = (
        string_map(old.get("environment")),
        string_map(new.get("environment")),
    );
    let keys: BTreeSet<&String> = old_env.keys().chain(new_env.keys()).collect();
    for k in keys {
        match (old_env.get(k), new_env.get(k)) {
            (None, Some(v)) => out.push(format!("env +{k}={v}")),
            (Some(_), None) => out.push(format!("env -{k}")),
            (Some(a), Some(b)) if a != b => out.push(format!("env {k}: {a} → {b}")),
            _ => {}
        }
    }

    let (old_sec, new_sec) = (
        string_map(old_secret.get("environment")),
        string_map(new_secret.get("environment")),
    );
    let keys: BTreeSet<&String> = old_sec.keys().chain(new_sec.keys()).collect();
    let changed: Vec<&str> = keys
        .into_iter()
        .filter(|k| old_sec.get(*k) != new_sec.get(*k))
        .map(String::as_str)
        .collect();
    if !changed.is_empty() {
        out.push(format!("secret env changed: {}", changed.join(", ")));
    }

    for field in ["ports", "volumes", "healthcheck", "depends_on", "deploy"] {
        if old.get(field) != new.get(field) {
            out.push(format!(
                "{field} {} → {}",
                compact(old.get(field)),
                compact(new.get(field))
            ));
        }
    }
    out
}

/// Diffs `desired` against the files last applied (`None` when nothing was applied).
pub fn diff(
    applied: Option<(&str, &str)>,
    desired: &Rendered,
) -> Result<Plan, serde_yaml_ng::Error> {
    let first_apply = applied.is_none();
    let (old_compose, old_secrets) = applied.unwrap_or(("", ""));
    let (old, new) = (
        services(&parse(old_compose)?),
        services(&parse(&desired.compose)?),
    );
    let (old_sec, new_sec) = (
        services(&parse(old_secrets)?),
        services(&parse(&desired.secrets)?),
    );

    let mut changes = Vec::new();
    for (name, svc) in &new {
        match old.get(name) {
            None => changes.push(Change::Add {
                service: name.clone(),
                image: svc
                    .get("image")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            }),
            Some(prev) => {
                let details = service_details(
                    prev,
                    svc,
                    old_sec.get(name).unwrap_or(&Value::Null),
                    new_sec.get(name).unwrap_or(&Value::Null),
                );
                if !details.is_empty() {
                    changes.push(Change::Update {
                        service: name.clone(),
                        details,
                    });
                }
            }
        }
    }
    for name in old.keys().filter(|n| !new.contains_key(*n)) {
        changes.push(Change::Remove {
            service: name.clone(),
        });
    }
    Ok(Plan {
        first_apply,
        changes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::render::render;
    use crate::compose::render::tests::sample;

    #[test]
    fn first_apply_adds_everything() {
        let r = render(&sample()).unwrap();
        let plan = diff(None, &r).unwrap();
        assert!(plan.first_apply);
        let added: Vec<&str> = plan.changes.iter().map(Change::service).collect();
        assert_eq!(added, ["orders-api", "postgres"]);
    }

    #[test]
    fn no_changes_when_nothing_differs() {
        let r = render(&sample()).unwrap();
        assert!(diff(Some((&r.compose, &r.secrets)), &r).unwrap().is_empty());
    }

    #[test]
    fn reports_updates_without_secret_values() {
        let before = render(&sample()).unwrap();
        let mut input = sample();
        input.services[0].instance.image = "postgres:17.2".into();
        input.services[0]
            .secrets
            .insert("POSTGRES_PASSWORD".into(), "new-secret".into());
        input.services[1]
            .instance
            .env
            .insert("LOG_LEVEL".into(), "warn".into());
        input.services[1].instance.publish.clear();
        let after = render(&input).unwrap();

        let plan = diff(Some((&before.compose, &before.secrets)), &after).unwrap();
        let text = serde_json::to_string(&plan).unwrap();
        assert!(
            !text.contains("new-secret") && !text.contains("pa$"),
            "{text}"
        );
        let details = |svc: &str| match plan.changes.iter().find(|c| c.service() == svc) {
            Some(Change::Update { details, .. }) => details.clone(),
            other => panic!("{other:?}"),
        };
        assert!(
            details("postgres").contains(&r#"image "postgres:16.6" → "postgres:17.2""#.to_owned())
        );
        assert!(details("postgres").contains(&"secret env changed: POSTGRES_PASSWORD".to_owned()));
        // orders-api reads the password by reference, so its secret env changes too.
        let api = details("orders-api");
        assert!(
            api.contains(&"env LOG_LEVEL: debug → warn".to_owned()),
            "{api:?}"
        );
        assert!(api.iter().any(|d| d.starts_with("ports ")), "{api:?}");
    }

    #[test]
    fn removed_services_are_reported() {
        let before = render(&sample()).unwrap();
        let mut input = sample();
        input.services.truncate(1);
        input.services[0].spec.depends_on.clear();
        let plan = diff(
            Some((&before.compose, &before.secrets)),
            &render(&input).unwrap(),
        )
        .unwrap();
        assert_eq!(
            plan.changes,
            [Change::Remove {
                service: "orders-api".into()
            }]
        );
    }
}
