use crate::{
    config::valid_label,
    files::{sha256, valid_hash},
    protocol::{DISCOVERY_PORT, PORT},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    time::Instant,
};

#[derive(Clone, Debug)]
pub struct Network {
    pub address: Ipv4Addr,
    pub prefix: u8,
    pub gateway: Ipv4Addr,
    pub router_mac: String,
    pub fingerprint: String,
    pub adapter: String,
}

fn mask(prefix: u8) -> u32 {
    u32::MAX << (32 - prefix)
}

impl Network {
    pub fn from_parts(
        address: Ipv4Addr,
        prefix: u8,
        gateway: Ipv4Addr,
        mac: &str,
        adapter: String,
    ) -> Result<Self> {
        ensure!(
            address.is_private() && gateway.is_private(),
            "Only RFC1918 private IPv4 LANs are supported"
        );
        ensure!((8..=30).contains(&prefix), "Unsupported LAN prefix");
        let network = u32::from(address) & mask(prefix);
        ensure!(
            u32::from(gateway) & mask(prefix) == network,
            "Router must be on the local subnet"
        );
        let router_mac = mac.replace(':', "-").to_ascii_lowercase();
        let octets: Vec<&str> = router_mac.split('-').collect();
        ensure!(
            octets.len() == 6
                && octets
                    .iter()
                    .all(|p| p.len() == 2 && u8::from_str_radix(p, 16).is_ok()),
            "Invalid router MAC address"
        );
        let raw_mac = hex::decode(router_mac.replace('-', ""))?;
        ensure!(
            raw_mac.iter().any(|b| *b != 0) && raw_mac[0] & 1 == 0,
            "Missing or multicast router MAC"
        );
        let fingerprint = sha256(
            format!("twilight-router/v1|{gateway}|{router_mac}|{network:08x}/{prefix}").as_bytes(),
        );
        Ok(Self {
            address,
            prefix,
            gateway,
            router_mac,
            fingerprint,
            adapter,
        })
    }

    pub fn allows(&self, ip: Ipv4Addr) -> bool {
        let number = u32::from(ip);
        let net = u32::from(self.address) & mask(self.prefix);
        ip.is_private()
            && ip != self.address
            && ip != self.gateway
            && number & mask(self.prefix) == net
            && number != net
            && number != (net | !mask(self.prefix))
    }

    pub fn broadcast(&self) -> Ipv4Addr {
        Ipv4Addr::from((u32::from(self.address) & mask(self.prefix)) | !mask(self.prefix))
    }

    pub fn discovery_socket(&self) -> Result<UdpSocket> {
        let socket = UdpSocket::bind((self.address, DISCOVERY_PORT))?;
        socket.set_broadcast(true)?;
        socket.set_nonblocking(true)?;
        Ok(socket)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Advertisement {
    pub magic: String,
    pub network: String,
    pub key: String,
    pub name: String,
    pub pairing: bool,
}

#[derive(Clone, Debug)]
pub struct Discovered {
    pub key: String,
    pub name: String,
    pub address: SocketAddr,
    pub pairing: bool,
    pub seen: Instant,
}

pub fn decode_advertisement(
    bytes: &[u8],
    source: SocketAddr,
    network: &Network,
    own_key: &str,
) -> Result<Discovered> {
    ensure!(bytes.len() <= 512, "Oversized discovery announcement");
    let SocketAddr::V4(source) = source else {
        anyhow::bail!("IPv6 discovery is not enabled");
    };
    ensure!(
        source.port() == DISCOVERY_PORT && network.allows(*source.ip()),
        "Discovery source is not a direct LAN peer"
    );
    let ad: Advertisement = serde_json::from_slice(bytes)?;
    ensure!(
        ad.magic == "twilightProto/1" && ad.network == network.fingerprint,
        "Discovery network mismatch"
    );
    ensure!(
        valid_hash(&ad.key) && ad.key != own_key && valid_label(&ad.name),
        "Invalid discovery identity"
    );
    Ok(Discovered {
        key: ad.key,
        name: ad.name,
        address: SocketAddr::from((*source.ip(), PORT)),
        pairing: ad.pairing,
        seen: Instant::now(),
    })
}

#[cfg(windows)]
pub fn probe() -> Result<Network> {
    use std::{os::windows::process::CommandExt, path::PathBuf, process::Command};
    #[derive(Deserialize)]
    struct Probe {
        address: Ipv4Addr,
        prefix: u8,
        gateway: Ipv4Addr,
        mac: String,
        adapter: String,
    }
    let script = r#"
$ErrorActionPreference='Stop'
$routes = @(Get-NetRoute -DestinationPrefix '0.0.0.0/0' -AddressFamily IPv4 |
    Where-Object { $_.NextHop -ne '0.0.0.0' } |
    Sort-Object @{Expression={ $_.RouteMetric + (Get-NetIPInterface -InterfaceIndex $_.InterfaceIndex -AddressFamily IPv4).InterfaceMetric }})
$chosen = $null
foreach ($route in $routes) {
    $adapter = Get-NetAdapter -InterfaceIndex $route.InterfaceIndex -ErrorAction Stop
    if ($adapter.Status -eq 'Up' -and $adapter.HardwareInterface -and -not $adapter.Virtual) { $chosen = $route; break }
}
if ($null -eq $chosen) { throw 'No physical LAN with a default router is available. VPN-only connections are refused.' }
$profile = Get-NetConnectionProfile -InterfaceIndex $chosen.InterfaceIndex
if ($profile.NetworkCategory -ne 'Private') { throw 'Set your trusted Windows network profile to Private before enabling sync.' }
$ip = @(Get-NetIPAddress -InterfaceIndex $chosen.InterfaceIndex -AddressFamily IPv4 |
    Where-Object { $_.AddressState -eq 'Preferred' -and -not $_.SkipAsSource })[0]
if ($null -eq $ip) { throw 'The selected LAN has no usable IPv4 address.' }
$ping = [System.Net.NetworkInformation.Ping]::new()
try { $null = $ping.Send($chosen.NextHop, 300) } finally { $ping.Dispose() }
$neighbor = @(Get-NetNeighbor -InterfaceIndex $chosen.InterfaceIndex -IPAddress $chosen.NextHop |
    Where-Object { $_.State -ne 'Unreachable' -and $_.State -ne 'Incomplete' })[0]
if ($null -eq $neighbor) { throw 'Cannot resolve router MAC. Network trust is refused.' }
@{ address=$ip.IPAddress; prefix=$ip.PrefixLength; gateway=$chosen.NextHop; mac=$neighbor.LinkLayerAddress; adapter=$chosen.InterfaceAlias } | ConvertTo-Json -Compress
"#;
    let windows = std::env::var_os("SystemRoot").context("SystemRoot is unavailable")?;
    let output = Command::new(
        PathBuf::from(windows).join(r"System32\WindowsPowerShell\v1.0\powershell.exe"),
    )
    .args(["-NoProfile", "-NonInteractive", "-Command", script])
    .creation_flags(0x08000000)
    .output()?;
    ensure!(
        output.status.success(),
        "LAN check failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let data: Probe =
        serde_json::from_slice(&output.stdout).context("Reading Windows network information")?;
    Network::from_parts(
        data.address,
        data.prefix,
        data.gateway,
        &data.mac,
        data.adapter,
    )
}

#[cfg(not(windows))]
pub fn probe() -> Result<Network> {
    anyhow::bail!("Router fingerprinting currently requires Windows")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_router_same_subnet_same_fingerprint() -> Result<()> {
        let a = Network::from_parts(
            "192.168.1.2".parse()?,
            24,
            "192.168.1.1".parse()?,
            "02-AA-BB-CC-DD-EE",
            "A".into(),
        )?;
        let b = Network::from_parts(
            "192.168.1.8".parse()?,
            24,
            "192.168.1.1".parse()?,
            "02:aa:bb:cc:dd:ee",
            "B".into(),
        )?;
        assert_eq!(a.fingerprint, b.fingerprint);
        assert!(a.allows(b.address));
        for denied in [
            "192.168.2.8",
            "8.8.8.8",
            "127.0.0.1",
            "192.168.1.255",
            "192.168.1.1",
            "192.168.1.0",
            "192.168.1.2",
        ] {
            assert!(!a.allows(denied.parse()?));
        }
        let other = Network::from_parts(b.address, 24, b.gateway, "02-AA-BB-CC-DD-EF", "B".into())?;
        assert_ne!(a.fingerprint, other.fingerprint);
        Ok(())
    }

    #[test]
    fn refuses_public_or_unresolved_routers() -> Result<()> {
        assert!(
            Network::from_parts(
                "8.8.8.8".parse()?,
                24,
                "8.8.8.1".parse()?,
                "02-AA-BB-CC-DD-EE",
                "X".into()
            )
            .is_err()
        );
        assert!(
            Network::from_parts(
                "192.168.1.2".parse()?,
                24,
                "192.168.2.1".parse()?,
                "02-AA-BB-CC-DD-EE",
                "X".into()
            )
            .is_err()
        );
        assert!(
            Network::from_parts(
                "192.168.1.2".parse()?,
                24,
                "192.168.1.1".parse()?,
                "00-00-00-00-00-00",
                "X".into()
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn discovery_requires_the_same_tag_and_direct_subnet() -> Result<()> {
        let network = Network::from_parts(
            "192.168.1.2".parse()?,
            24,
            "192.168.1.1".parse()?,
            "02-AA-BB-CC-DD-EE",
            "LAN".into(),
        )?;
        let mut advertisement = Advertisement {
            magic: "twilightProto/1".into(),
            network: network.fingerprint.clone(),
            key: "b".repeat(64),
            name: "Other device".into(),
            pairing: true,
        };
        let own = "a".repeat(64);
        let valid_source = SocketAddr::from(("192.168.1.3".parse::<Ipv4Addr>()?, DISCOVERY_PORT));
        let bytes = serde_json::to_vec(&advertisement)?;
        let device = decode_advertisement(&bytes, valid_source, &network, &own)?;
        assert_eq!(device.address.port(), PORT);
        for source in ["8.8.8.8:42816", "192.168.2.3:42816", "192.168.1.2:42816"] {
            assert!(decode_advertisement(&bytes, source.parse()?, &network, &own).is_err());
        }
        advertisement.network = "f".repeat(64);
        assert!(
            decode_advertisement(
                &serde_json::to_vec(&advertisement)?,
                valid_source,
                &network,
                &own
            )
            .is_err()
        );
        assert!(decode_advertisement(&vec![0; 513], valid_source, &network, &own).is_err());
        Ok(())
    }
}
