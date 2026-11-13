//! Host port policy: the range ikit hands host ports out from, and ports it never hands out.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// One port or an inclusive span of ports: `5432` or `9000-9100`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PortRange {
    pub first: u16,
    pub last: u16,
}

impl PortRange {
    pub const fn new(first: u16, last: u16) -> Self {
        Self { first, last }
    }

    pub const fn one(port: u16) -> Self {
        Self::new(port, port)
    }

    pub fn contains(&self, port: u16) -> bool {
        (self.first..=self.last).contains(&port)
    }

    pub fn overlaps(&self, other: &PortRange) -> bool {
        self.first <= other.last && other.first <= self.last
    }
}

impl fmt::Display for PortRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.first == self.last {
            write!(f, "{}", self.first)
        } else {
            write!(f, "{}-{}", self.first, self.last)
        }
    }
}

impl FromStr for PortRange {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || {
            format!("invalid port `{s}` (expected a port such as 5432 or a span such as 9000-9100)")
        };
        let port = |p: &str| match p.trim().parse::<u16>() {
            Ok(n) if n > 0 => Ok(n),
            _ => Err(invalid()),
        };
        let r = match s.split_once('-') {
            Some((a, b)) => Self::new(port(a)?, port(b)?),
            None => Self::one(port(s)?),
        };
        if r.first > r.last {
            return Err(invalid());
        }
        Ok(r)
    }
}

impl Serialize for PortRange {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if self.first == self.last {
            s.serialize_u16(self.first)
        } else {
            s.serialize_str(&self.to_string())
        }
    }
}

impl<'de> Deserialize<'de> for PortRange {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Num(u64),
            Text(String),
        }
        let text = match Raw::deserialize(d)? {
            Raw::Num(n) => n.to_string(),
            Raw::Text(t) => t,
        };
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// Ports ikit never hands out on its own, whatever the org config says: the privileged
/// ports (SSH, HTTP, HTTPS, …) and ZeroTier's.
pub const BUILTIN_RESERVED: [(PortRange, &str); 2] = [
    (
        PortRange::new(1, 1023),
        "privileged ports (ssh, http, https, …)",
    ),
    (PortRange::one(9993), "ZeroTier"),
];

/// The `ports:` section of `config.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PortDefaults {
    /// Host ports handed out to services, `[first, last]`.
    pub range: [u16; 2],
    /// Ports no service may publish and ikit never hands out, e.g. `[5432, "9000-9100"]`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reserved: Vec<PortRange>,
    /// Check the target's listening sockets and container ports before picking or applying.
    pub probe: bool,
}

impl Default for PortDefaults {
    fn default() -> Self {
        Self {
            range: [10000, 19999],
            reserved: Vec::new(),
            probe: true,
        }
    }
}

impl PortDefaults {
    pub(crate) fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub fn range(&self) -> PortRange {
        PortRange::new(self.range[0], self.range[1])
    }

    /// Why `port` is reserved by the org config, if it is.
    pub fn reserved_by_org(&self, port: u16) -> Option<PortRange> {
        self.reserved.iter().copied().find(|r| r.contains(port))
    }

    /// Why ikit must not hand out `port`: an org reservation or a built-in one.
    pub fn reserved_reason(&self, port: u16) -> Option<String> {
        if let Some(r) = self.reserved_by_org(port) {
            return Some(format!("reserved in config.yaml (ports.reserved: {r})"));
        }
        BUILTIN_RESERVED
            .iter()
            .find(|(r, _)| r.contains(port))
            .map(|(r, why)| format!("reserved by ikit ({r}: {why})"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ports_and_spans() {
        assert_eq!("5432".parse::<PortRange>().unwrap(), PortRange::one(5432));
        assert_eq!(
            "9000-9100".parse::<PortRange>().unwrap(),
            PortRange::new(9000, 9100)
        );
        for bad in ["0", "9100-9000", "x", "1-", "70000"] {
            assert!(bad.parse::<PortRange>().is_err(), "{bad}");
        }
    }

    #[test]
    fn reads_numbers_and_strings_from_yaml() {
        let p: PortDefaults =
            serde_yaml_ng::from_str("reserved: [5432, 9000-9100, \"6379\"]").unwrap();
        assert_eq!(
            p.reserved,
            [
                PortRange::one(5432),
                PortRange::new(9000, 9100),
                PortRange::one(6379)
            ]
        );
        assert_eq!(p.range, [10000, 19999]);
        let back = serde_yaml_ng::to_string(&p).unwrap();
        assert!(
            back.contains("- 5432") && back.contains("9000-9100"),
            "{back}"
        );
    }

    #[test]
    fn explains_reservations() {
        let p = PortDefaults {
            reserved: vec![PortRange::new(9000, 9100)],
            ..PortDefaults::default()
        };
        assert!(p.reserved_reason(9050).unwrap().contains("ports.reserved"));
        assert!(p.reserved_reason(22).unwrap().contains("privileged"));
        assert!(p.reserved_reason(9993).unwrap().contains("ZeroTier"));
        assert!(p.reserved_reason(30000).is_none());
        assert!(p.reserved_by_org(22).is_none());
    }
}
