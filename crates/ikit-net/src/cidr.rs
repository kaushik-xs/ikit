use std::fmt;
use std::net::Ipv4Addr;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::NetError;

/// An IPv4 network such as `10.147.17.0/24`: a ZeroTier network's managed route, with the
/// addresses in between assigned to members.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Ipv4Cidr {
    network: Ipv4Addr,
    prefix: u8,
}

impl Ipv4Cidr {
    pub fn network(&self) -> Ipv4Addr {
        self.network
    }

    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    fn mask(&self) -> u32 {
        u32::MAX
            .checked_shl(32 - u32::from(self.prefix))
            .unwrap_or(0)
    }

    /// The range members get addresses from: everything but the network and broadcast
    /// addresses.
    pub fn pool(&self) -> (Ipv4Addr, Ipv4Addr) {
        let base = u32::from(self.network);
        let last = base | !self.mask();
        (Ipv4Addr::from(base + 1), Ipv4Addr::from(last - 1))
    }

    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & self.mask() == u32::from(self.network)
    }
}

impl FromStr for Ipv4Cidr {
    type Err = NetError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = |why: &str| NetError::Invalid(format!("invalid IPv4 CIDR `{s}`: {why}"));
        let (ip, prefix) = s
            .split_once('/')
            .ok_or_else(|| invalid("expected ADDRESS/PREFIX, e.g. 10.147.17.0/24"))?;
        let ip: Ipv4Addr = ip.parse().map_err(|_| invalid("bad address"))?;
        let prefix: u8 = prefix.parse().map_err(|_| invalid("bad prefix"))?;
        if !(8..=30).contains(&prefix) {
            return Err(invalid("the prefix must be between 8 and 30"));
        }
        let cidr = Self {
            network: ip,
            prefix,
        };
        if u32::from(ip) & cidr.mask() != u32::from(ip) {
            return Err(invalid("host bits are set; use the network address"));
        }
        Ok(cidr)
    }
}

impl TryFrom<String> for Ipv4Cidr {
    type Error = NetError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<Ipv4Cidr> for String {
    fn from(c: Ipv4Cidr) -> Self {
        c.to_string()
    }
}

impl fmt::Display for Ipv4Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_computes_the_pool() {
        let c: Ipv4Cidr = "10.147.17.0/24".parse().unwrap();
        assert_eq!(
            c.pool(),
            (
                Ipv4Addr::new(10, 147, 17, 1),
                Ipv4Addr::new(10, 147, 17, 254)
            )
        );
        assert!(c.contains(Ipv4Addr::new(10, 147, 17, 9)));
        assert!(!c.contains(Ipv4Addr::new(10, 147, 18, 9)));
        assert_eq!(c.to_string(), "10.147.17.0/24");

        let c: Ipv4Cidr = "172.30.0.0/16".parse().unwrap();
        assert_eq!(c.pool().1, Ipv4Addr::new(172, 30, 255, 254));
    }

    #[test]
    fn rejects_bad_input() {
        for s in [
            "10.147.17.0",
            "10.147.17.5/24",
            "10.0.0.0/31",
            "10.0.0.0/4",
            "x/24",
        ] {
            assert!(s.parse::<Ipv4Cidr>().is_err(), "{s}");
        }
    }
}
