//! Registry versions and the constraints services use to pick one.

use std::fmt;
use std::str::FromStr;

use semver::{Version, VersionReq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Parses a registry version, accepting the short forms image tags often use:
/// `16` and `16.4` become `16.0.0` and `16.4.0`. A leading `v` is ignored.
pub fn parse_version(s: &str) -> Result<Version, VersionError> {
    let trimmed = s.trim().trim_start_matches('v');
    let (core, rest) = match trimmed.find(['-', '+']) {
        Some(i) => trimmed.split_at(i),
        None => (trimmed, ""),
    };
    let padded = match core.split('.').count() {
        1 => format!("{core}.0.0{rest}"),
        2 => format!("{core}.0{rest}"),
        _ => trimmed.to_owned(),
    };
    Version::parse(&padded).map_err(|e| VersionError::InvalidVersion {
        value: s.to_owned(),
        reason: e.to_string(),
    })
}

/// How a service picks a registry version.
///
/// - `latest`: the newest stable, non-deprecated version.
/// - `1.4.2`: exactly that version (deprecated versions are still allowed when pinned).
/// - `16` / `16.4`: the newest `16.x.y` / `16.4.y`.
/// - Anything with an operator (`^1.4`, `~1.4`, `>=1.2, <2`) is a semver requirement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionConstraint {
    Latest,
    Exact(Version),
    Req { raw: String, req: VersionReq },
}

impl VersionConstraint {
    /// Picks the best of `candidates` (version, deprecated) for this constraint.
    pub fn resolve<'a, I>(&self, candidates: I) -> Option<&'a Version>
    where
        I: IntoIterator<Item = (&'a Version, bool)>,
    {
        let eligible = candidates.into_iter().filter(|(v, deprecated)| match self {
            Self::Latest => !deprecated && v.pre.is_empty(),
            Self::Exact(want) => *v == want,
            Self::Req { req, .. } => !deprecated && req.matches(v),
        });
        eligible.map(|(v, _)| v).max()
    }

    pub fn matches(&self, version: &Version) -> bool {
        match self {
            Self::Latest => true,
            Self::Exact(want) => version == want,
            Self::Req { req, .. } => req.matches(version),
        }
    }
}

impl FromStr for VersionConstraint {
    type Err = VersionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() || s == "latest" || s == "*" {
            return Ok(Self::Latest);
        }
        let invalid = |reason: String| VersionError::InvalidConstraint {
            value: s.to_owned(),
            reason,
        };
        let has_operator =
            s.starts_with(['^', '~', '=', '<', '>']) || s.contains(',') || s.contains('*');
        if !has_operator {
            let parts = s
                .trim_start_matches('v')
                .split(['-', '+'])
                .next()
                .unwrap_or("")
                .split('.')
                .count();
            if parts >= 3 {
                return parse_version(s)
                    .map(Self::Exact)
                    .map_err(|e| invalid(e.to_string()));
            }
            // A partial version is a prefix: `16` = 16.x.y, `16.4` = 16.4.y.
            let req = VersionReq::parse(&format!("~{s}")).map_err(|e| invalid(e.to_string()))?;
            return Ok(Self::Req {
                raw: s.to_owned(),
                req,
            });
        }
        let req = VersionReq::parse(s).map_err(|e| invalid(e.to_string()))?;
        Ok(Self::Req {
            raw: s.to_owned(),
            req,
        })
    }
}

impl fmt::Display for VersionConstraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Latest => f.write_str("latest"),
            Self::Exact(v) => write!(f, "{v}"),
            Self::Req { raw, .. } => f.write_str(raw),
        }
    }
}

impl Serialize for VersionConstraint {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for VersionConstraint {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// `name@constraint`, as used by `--services` and `depends_on`. A bare name means `latest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    pub name: String,
    pub constraint: VersionConstraint,
}

impl FromStr for Dependency {
    type Err = VersionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (name, constraint) = match s.split_once('@') {
            Some((n, c)) => (n.trim(), c.parse()?),
            None => (s.trim(), VersionConstraint::Latest),
        };
        if !crate::is_slug(name) {
            return Err(VersionError::InvalidName(name.to_owned()));
        }
        Ok(Self {
            name: name.to_owned(),
            constraint,
        })
    }
}

impl Dependency {
    /// Whether any `(registry service, version)` in `installed` meets this dependency.
    pub fn satisfied_by<'a>(
        &self,
        installed: impl IntoIterator<Item = (&'a str, &'a Version)>,
    ) -> bool {
        installed
            .into_iter()
            .any(|(name, v)| name == self.name && self.constraint.matches(v))
    }
}

impl fmt::Display for Dependency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.name, self.constraint)
    }
}

impl Serialize for Dependency {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Dependency {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VersionError {
    #[error("invalid version `{value}`: {reason}")]
    InvalidVersion { value: String, reason: String },
    #[error("invalid version constraint `{value}`: {reason}")]
    InvalidConstraint { value: String, reason: String },
    #[error("invalid service name `{0}`: use {rule}", rule = crate::SLUG_RULE)]
    InvalidName(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        parse_version(s).unwrap()
    }

    fn resolve(constraint: &str, candidates: &[(&str, bool)]) -> Option<String> {
        let versions: Vec<(Version, bool)> = candidates.iter().map(|(s, d)| (v(s), *d)).collect();
        let c: VersionConstraint = constraint.parse().unwrap();
        c.resolve(versions.iter().map(|(v, d)| (v, *d)))
            .map(|v| v.to_string())
    }

    #[test]
    fn parses_short_versions() {
        assert_eq!(v("16"), Version::new(16, 0, 0));
        assert_eq!(v("16.4"), Version::new(16, 4, 0));
        assert_eq!(v("v1.2.3"), Version::new(1, 2, 3));
        assert_eq!(v("1.2-rc.1").to_string(), "1.2.0-rc.1");
        assert!(parse_version("sixteen").is_err());
    }

    #[test]
    fn resolves_constraints() {
        let candidates = [
            ("16.3.0", false),
            ("16.4.0", false),
            ("17.0.0", false),
            ("17.1.0", true),
            ("18.0.0-rc.1", false),
        ];
        assert_eq!(resolve("latest", &candidates).as_deref(), Some("17.0.0"));
        assert_eq!(resolve("16", &candidates).as_deref(), Some("16.4.0"));
        assert_eq!(resolve("16.3", &candidates).as_deref(), Some("16.3.0"));
        assert_eq!(resolve("^16", &candidates).as_deref(), Some("16.4.0"));
        assert_eq!(resolve(">=16, <18", &candidates).as_deref(), Some("17.0.0"));
        // Pinning still reaches a deprecated version; ranges skip it.
        assert_eq!(resolve("17.1.0", &candidates).as_deref(), Some("17.1.0"));
        assert_eq!(resolve("17", &candidates).as_deref(), Some("17.0.0"));
        assert_eq!(resolve("19", &candidates), None);
    }

    #[test]
    fn constraint_display_round_trips() {
        for s in ["latest", "1.4.2", "16", "^1.4", ">=1.2, <2"] {
            assert_eq!(s.parse::<VersionConstraint>().unwrap().to_string(), s);
        }
    }

    #[test]
    fn parses_dependencies() {
        let d: Dependency = "postgres@^16".parse().unwrap();
        assert_eq!(d.name, "postgres");
        assert_eq!(d.to_string(), "postgres@^16");
        assert_eq!(
            "redis".parse::<Dependency>().unwrap().constraint,
            VersionConstraint::Latest
        );
        assert!("Postgres@16".parse::<Dependency>().is_err());
        assert!("postgres@not-a-version".parse::<Dependency>().is_err());
    }
}
