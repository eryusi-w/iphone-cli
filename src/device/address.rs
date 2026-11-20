//! A Wi-Fi address includes the IPv6 interface needed for link-local peers.

use std::{
    fmt,
    io::Write,
    net::{IpAddr, SocketAddr, SocketAddrV6},
    path::Path,
    str::FromStr,
};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WifiAddress {
    pub ip: IpAddr,
    pub scope_id: u32,
}

impl WifiAddress {
    pub fn new(ip: IpAddr, scope_id: u32) -> Result<Self> {
        if ip.is_unspecified() || ip.is_multicast() {
            bail!("use a unicast iPhone address");
        }
        if ip.is_ipv4() && scope_id != 0 {
            bail!("an IPv4 address cannot have an interface scope");
        }
        if matches!(ip, IpAddr::V6(v) if v.is_unicast_link_local()) && scope_id == 0 {
            bail!("a link-local IPv6 address needs a numeric interface, e.g. fe80::1%12");
        }
        Ok(Self { ip, scope_id })
    }

    pub fn socket_addr(self, port: u16) -> SocketAddr {
        match self.ip {
            IpAddr::V4(_) => SocketAddr::new(self.ip, port),
            IpAddr::V6(ip) => SocketAddr::V6(SocketAddrV6::new(ip, port, 0, self.scope_id)),
        }
    }
}

impl FromStr for WifiAddress {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        let (ip, scope) = match s.split_once('%') {
            Some((ip, scope)) => (ip.parse()?, scope.parse()?),
            None => (s.parse()?, 0),
        };
        Self::new(ip, scope)
    }
}

impl TryFrom<String> for WifiAddress {
    type Error = anyhow::Error;
    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

impl From<WifiAddress> for String {
    fn from(value: WifiAddress) -> Self {
        value.to_string()
    }
}

impl fmt::Display for WifiAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.ip)?;
        if self.scope_id != 0 {
            write!(f, "%{}", self.scope_id)?;
        }
        Ok(())
    }
}

pub(crate) fn valid_udid(udid: &str) -> bool {
    !udid.is_empty()
        && udid.len() <= 128
        && udid.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
}

pub(crate) fn cached(udid: &str) -> Option<WifiAddress> {
    if !valid_udid(udid) {
        return None;
    }
    std::fs::read_to_string(paths::state_dir().join(format!("{udid}.addr")))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Atomic replacement is shared by manual registration and verified discovery.
pub(crate) fn save(state: &Path, udid: &str, address: WifiAddress) -> std::io::Result<()> {
    if !valid_udid(udid) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid UDID",
        ));
    }
    paths::ensure_private_dir(state)?;
    let temporary = state.join(format!("{udid}.addr.{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = paths::private_file()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(address.to_string().as_bytes())?;
        drop(file);
        std::fs::rename(&temporary, state.join(format!("{udid}.addr")))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_cache_strings_and_scoped_ipv6_round_trip() {
        for value in ["192.0.2.1", "2001:db8::1", "fe80::1234%12"] {
            let addr: WifiAddress = value.parse().unwrap();
            assert_eq!(addr.to_string(), value);
            assert_eq!(
                serde_json::from_str::<WifiAddress>(&serde_json::to_string(&addr).unwrap())
                    .unwrap(),
                addr
            );
        }
        let addr: WifiAddress = "fe80::1234%12".parse().unwrap();
        assert_eq!(addr.socket_addr(62078).to_string(), "[fe80::1234%12]:62078");
    }

    #[test]
    fn invalid_or_unscoped_addresses_are_not_connection_candidates() {
        for value in [
            "0.0.0.0",
            "::",
            "224.0.0.251",
            "ff02::fb%12",
            "fe80::1",
            "fe80::1%0",
            "192.0.2.1%12",
            "fe80::1%name",
            "host.local",
        ] {
            assert!(value.parse::<WifiAddress>().is_err(), "{value}");
        }
    }
}
