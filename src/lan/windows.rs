use super::Network;
use anyhow::{Context, Result, ensure};
use std::{ffi::c_void, net::Ipv4Addr, ptr};
use windows::{
    Win32::{
        Networking::NetworkListManager::{
            INetworkListManager, NLM_NETWORK_CATEGORY_PRIVATE, NetworkListManager,
        },
        System::Com::{CLSCTX_ALL, CoCreateInstance},
    },
    core::{GUID, HRESULT},
};
use windows_sys::Win32::{
    Foundation::{HANDLE, INVALID_HANDLE_VALUE, RPC_E_CHANGED_MODE},
    NetworkManagement::{IpHelper::*, Ndis::IfOperStatusUp},
    Networking::WinSock::{
        AF_INET, IpDadStatePreferred, NlnsIncomplete, NlnsUnreachable, SOCKADDR_INET,
    },
    System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize},
};

const NO_LAN: &str =
    "No physical LAN with a default router is available. VPN-only connections are refused.";
const NOT_PRIVATE: &str =
    "Set your trusted Windows network profile to Private before enabling sync.";
const NO_ADDRESS: &str = "The selected LAN has no usable IPv4 address.";
const NO_NEIGHBOR: &str = "Cannot resolve router MAC. Network trust is refused.";

struct Apartment(bool);

impl Apartment {
    fn initialize() -> Result<Self> {
        let result = unsafe { CoInitializeEx(ptr::null(), COINIT_MULTITHREADED as u32) };
        if result != RPC_E_CHANGED_MODE {
            HRESULT(result)
                .ok()
                .context("Initializing LAN profile COM")?;
        }
        Ok(Self(result >= 0))
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

struct MibTable(*mut c_void);

impl Drop for MibTable {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { FreeMibTable(self.0) };
        }
    }
}

struct Icmp(HANDLE);

impl Drop for Icmp {
    fn drop(&mut self) {
        if unsafe { IcmpCloseHandle(self.0) } == 0 {
            eprintln!(
                "Cannot close LAN echo handle: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

fn check(status: u32) -> Result<()> {
    if status != 0 {
        return Err(std::io::Error::from_raw_os_error(status as i32))
            .context("Reading LAN information");
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct Route {
    interface: u32,
    gateway: Ipv4Addr,
    metric: u32,
    interface_metric: u32,
}

fn physical(adapter: &MIB_IF_ROW2) -> bool {
    // IP Helper defines HardwareInterface at bit 0 and ConnectorPresent at bit 2.
    adapter.OperStatus == IfOperStatusUp
        && adapter.InterfaceAndOperStatusFlags._bitfield & 0b101 == 0b101
}

fn select_route(
    routes: &[Route],
    mut adapter: impl FnMut(u32) -> Result<MIB_IF_ROW2>,
) -> Result<(Route, MIB_IF_ROW2)> {
    let mut routes: Vec<_> = routes
        .iter()
        .filter(|route| !route.gateway.is_unspecified())
        .collect();
    routes.sort_by_key(|route| u64::from(route.metric) + u64::from(route.interface_metric));
    for route in routes {
        let adapter = adapter(route.interface)?;
        if physical(&adapter) {
            return Ok((*route, adapter));
        }
    }
    anyhow::bail!(NO_LAN)
}

fn ipv4(address: &SOCKADDR_INET) -> Option<Ipv4Addr> {
    // The address family selects the active member of the Windows address union.
    (unsafe { address.si_family } == AF_INET)
        .then(|| Ipv4Addr::from(unsafe { address.Ipv4.sin_addr.S_un.S_addr }.to_ne_bytes()))
}

fn private_profile(adapter: &MIB_IF_ROW2) -> Result<()> {
    let guid = adapter.InterfaceGuid;
    let guid = GUID::from_values(guid.data1, guid.data2, guid.data3, guid.data4);
    let manager: INetworkListManager =
        unsafe { CoCreateInstance(&NetworkListManager, None, CLSCTX_ALL) }
            .context("Opening Windows network profiles")?;
    let connections = unsafe { manager.GetNetworkConnections() }?;
    let mut found = false;
    loop {
        let mut connection = [None];
        let mut fetched = 0;
        unsafe { connections.Next(&mut connection, Some(&mut fetched)) }?;
        if fetched == 0 {
            break;
        }
        let connection = connection[0]
            .take()
            .context("Missing Windows network connection")?;
        if unsafe { connection.GetAdapterId() }? == guid {
            found = true;
            let network = unsafe { connection.GetNetwork() }?;
            ensure!(
                unsafe { network.GetCategory() }? == NLM_NETWORK_CATEGORY_PRIVATE,
                NOT_PRIVATE
            );
        }
    }
    ensure!(found, NOT_PRIVATE);
    Ok(())
}

fn neighbor_mac(row: &MIB_IPNET_ROW2) -> Result<String> {
    ensure!(
        row.State != NlnsUnreachable && row.State != NlnsIncomplete,
        NO_NEIGHBOR
    );
    let length = usize::try_from(row.PhysicalAddressLength)?;
    let bytes = row.PhysicalAddress.get(..length).context(NO_NEIGHBOR)?;
    Ok(bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join("-"))
}

pub(super) fn probe() -> Result<Network> {
    let _apartment = Apartment::initialize()?;
    let mut raw = ptr::null_mut();
    let status = unsafe { GetIpForwardTable2(AF_INET, &mut raw) };
    let _routes = MibTable(raw.cast());
    check(status)?;
    ensure!(!raw.is_null(), "Windows returned no route table");
    let table = unsafe { &*raw };
    let rows =
        unsafe { std::slice::from_raw_parts(table.Table.as_ptr(), table.NumEntries as usize) };
    let mut routes = Vec::new();
    for row in rows {
        if row.DestinationPrefix.PrefixLength != 0
            || ipv4(&row.DestinationPrefix.Prefix) != Some(Ipv4Addr::UNSPECIFIED)
        {
            continue;
        }
        let gateway = ipv4(&row.NextHop).context("Default router is not IPv4")?;
        if gateway.is_unspecified() {
            continue;
        }
        let mut interface = MIB_IPINTERFACE_ROW {
            Family: AF_INET,
            InterfaceIndex: row.InterfaceIndex,
            ..Default::default()
        };
        check(unsafe { GetIpInterfaceEntry(&mut interface) })?;
        routes.push(Route {
            interface: row.InterfaceIndex,
            gateway,
            metric: row.Metric,
            interface_metric: interface.Metric,
        });
    }
    let (route, adapter) = select_route(&routes, |interface| {
        let mut row = MIB_IF_ROW2 {
            InterfaceIndex: interface,
            ..Default::default()
        };
        check(unsafe { GetIfEntry2(&mut row) })?;
        Ok(row)
    })?;
    private_profile(&adapter)?;
    let mut raw = ptr::null_mut();
    let status = unsafe { GetUnicastIpAddressTable(AF_INET, &mut raw) };
    let _addresses = MibTable(raw.cast());
    check(status)?;
    ensure!(!raw.is_null(), "Windows returned no IPv4 address table");
    let table = unsafe { &*raw };
    let addresses =
        unsafe { std::slice::from_raw_parts(table.Table.as_ptr(), table.NumEntries as usize) };
    let address = addresses
        .iter()
        .find(|row| {
            row.InterfaceIndex == route.interface
                && row.DadState == IpDadStatePreferred
                && !row.SkipAsSource
        })
        .context(NO_ADDRESS)?;
    let ip = ipv4(&address.Address).context(NO_ADDRESS)?;
    let handle = unsafe { IcmpCreateFile() };
    ensure!(
        handle != INVALID_HANDLE_VALUE && !handle.is_null(),
        "Creating LAN echo handle: {}",
        std::io::Error::last_os_error()
    );
    let icmp = Icmp(handle);
    let mut reply = [0u64; 16];
    unsafe {
        IcmpSendEcho(
            icmp.0,
            u32::from_ne_bytes(route.gateway.octets()),
            ptr::null(),
            0,
            ptr::null(),
            reply.as_mut_ptr().cast(),
            std::mem::size_of_val(&reply) as u32,
            300,
        );
    }
    let mut neighbor = MIB_IPNET_ROW2 {
        InterfaceIndex: route.interface,
        ..Default::default()
    };
    neighbor.Address.Ipv4.sin_family = AF_INET;
    neighbor.Address.Ipv4.sin_addr.S_un.S_addr = u32::from_ne_bytes(route.gateway.octets());
    ensure!(unsafe { GetIpNetEntry2(&mut neighbor) } == 0, NO_NEIGHBOR);
    let mac = neighbor_mac(&neighbor)?;
    let end = adapter
        .Alias
        .iter()
        .position(|ch| *ch == 0)
        .unwrap_or(adapter.Alias.len());
    Network::from_parts(
        ip,
        address.OnLinkPrefixLength,
        route.gateway,
        &mac,
        String::from_utf16(&adapter.Alias[..end])?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(interface: u32, metric: u32, interface_metric: u32) -> Route {
        Route {
            interface,
            gateway: Ipv4Addr::new(192, 168, 1, 1),
            metric,
            interface_metric,
        }
    }

    fn adapter(up: bool, hardware: bool, connector: bool) -> MIB_IF_ROW2 {
        let mut row = MIB_IF_ROW2 {
            OperStatus: if up { IfOperStatusUp } else { 2 },
            ..Default::default()
        };
        row.InterfaceAndOperStatusFlags._bitfield = u8::from(hardware) | (u8::from(connector) << 2);
        row
    }

    #[test]
    fn lowest_combined_metric_wins_without_overflow() -> Result<()> {
        let routes = [route(1, 1, 100), route(2, 20, 1), route(3, u32::MAX, 1)];
        assert_eq!(
            select_route(&routes, |_| Ok(adapter(true, true, true)))?
                .0
                .interface,
            2
        );
        Ok(())
    }

    #[test]
    fn skips_down_virtual_nonhardware_and_zero_next_hops() -> Result<()> {
        let mut routes: Vec<_> = (0..5).map(|i| route(i, i, 0)).collect();
        routes[0].gateway = Ipv4Addr::UNSPECIFIED;
        let (selected, _) = select_route(&routes, |i| {
            Ok(match i {
                0 => panic!("An on-link route must never be considered"),
                1 => adapter(false, true, true),
                2 => adapter(true, true, false),
                3 => adapter(true, false, true),
                _ => adapter(true, true, true),
            })
        })?;
        assert_eq!(selected.interface, 4);
        assert_eq!(
            select_route(&routes[..4], |_| Ok(adapter(false, true, true)))
                .err()
                .context("Expected refusal")?
                .to_string(),
            NO_LAN
        );
        Ok(())
    }

    #[test]
    fn unresolved_neighbors_are_refused() -> Result<()> {
        for state in [NlnsUnreachable, NlnsIncomplete] {
            let row = MIB_IPNET_ROW2 {
                State: state,
                ..Default::default()
            };
            assert_eq!(
                neighbor_mac(&row)
                    .err()
                    .context("Expected refusal")?
                    .to_string(),
                NO_NEIGHBOR
            );
        }
        let mut row = MIB_IPNET_ROW2 {
            State: 5,
            PhysicalAddressLength: 6,
            ..Default::default()
        };
        row.PhysicalAddress[..6].copy_from_slice(&[0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0xee]);
        assert_eq!(neighbor_mac(&row)?, "02-AA-BB-CC-DD-EE");
        Ok(())
    }
}
