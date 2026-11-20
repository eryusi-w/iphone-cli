//! Apple supplies discovery hints; it does not choose the Wi-Fi transport.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use idevice::{
    ReadWrite,
    usbmuxd::{Connection, RawPacket, UsbmuxdDevice},
};
use plist::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{address::WifiAddress, conn};
use crate::events;

#[derive(Debug, Clone)]
pub(crate) struct Device {
    pub mux: UsbmuxdDevice,
    pub address: Option<WifiAddress>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Discovery {
    pub devices: Vec<Device>,
    pub warnings: Vec<String>,
}

pub(crate) async fn read() -> Result<Discovery> {
    let addr = conn::mux_addr()?;
    events::step(
        "usbmuxd.ListDevices",
        serde_json::json!({}),
        Duration::from_secs(4),
        async {
            let mut socket = addr.to_socket().await.context("usbmuxd.connect")?;
            request(&mut *socket).await
        },
    )
    .await
}

// The dependency's get_devices drops IPv6 scope IDs and malformed entries.
// Read this one public usbmuxd message with bounds and retain both explicitly.
async fn request(socket: &mut dyn ReadWrite) -> Result<Discovery> {
    let req = plist::Dictionary::from_iter([
        ("MessageType".to_string(), Value::from("ListDevices")),
        ("ClientVersionString".to_string(), Value::from(conn::LABEL)),
        ("kLibUSBMuxVersion".to_string(), Value::from(3u64)),
    ]);
    let packet: Vec<u8> = RawPacket::new(req, 1, 8, 0).into();
    socket.write_all(&packet).await?;
    let mut header = [0; 16];
    socket.read_exact(&mut header).await?;
    let size = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
    if !(16..=16 * 1024 * 1024).contains(&size) {
        bail!("invalid ListDevices frame length {size}");
    }
    if u32::from_le_bytes(header[4..8].try_into().unwrap()) != 1
        || u32::from_le_bytes(header[8..12].try_into().unwrap()) != 8
        || u32::from_le_bytes(header[12..16].try_into().unwrap()) != 0
    {
        bail!("unexpected ListDevices response header");
    }
    let mut bytes = vec![0; size - 16];
    socket.read_exact(&mut bytes).await?;
    parse(&plist::from_bytes::<Value>(&bytes)?)
}

fn network_address(bytes: &[u8], interface: u32) -> Result<WifiAddress> {
    let family = match bytes {
        [16 | 28, family, ..] => u16::from(*family),
        [a, b, ..] => u16::from_le_bytes([*a, *b]),
        _ => bail!("missing socket address family"),
    };
    match family {
        2 if bytes.len() >= 16 => WifiAddress::new(
            Ipv4Addr::new(bytes[4], bytes[5], bytes[6], bytes[7]).into(),
            0,
        ),
        // Darwin, Linux and Windows AF_INET6 values, respectively.
        30 | 10 | 23 if bytes.len() >= 28 => {
            let mut ip: [u8; 16] = bytes[8..24].try_into().unwrap();
            let mut scope = u32::from_ne_bytes(bytes[24..28].try_into().unwrap());
            if scope == 0 {
                scope = interface;
            }
            // Some BSD socket addresses embed the link scope in the address.
            let embedded_scope = u16::from_be_bytes([ip[2], ip[3]]) as u32;
            if Ipv6Addr::from(ip).is_unicast_link_local()
                && embedded_scope != 0
                && (scope == 0 || scope == embedded_scope)
            {
                if scope == 0 {
                    scope = embedded_scope;
                }
                ip[2..4].fill(0);
            }
            WifiAddress::new(IpAddr::V6(Ipv6Addr::from(ip)), scope)
        }
        _ => bail!(
            "unsupported or truncated network socket address (family {family}, {} bytes)",
            bytes.len()
        ),
    }
}

fn device(v: &Value) -> Result<Device> {
    let v = v.as_dictionary().context("device is not a dictionary")?;
    let properties = v
        .get("Properties")
        .and_then(Value::as_dictionary)
        .context("missing Properties")?;
    let id = v
        .get("DeviceID")
        .and_then(Value::as_unsigned_integer)
        .context("missing DeviceID")?;
    let udid = properties
        .get("SerialNumber")
        .and_then(Value::as_string)
        .filter(|s| !s.is_empty())
        .context("missing SerialNumber")?;
    let connection = properties
        .get("ConnectionType")
        .and_then(Value::as_string)
        .context("missing ConnectionType")?;
    let address = if connection == "Network" {
        let bytes = properties
            .get("NetworkAddress")
            .and_then(Value::as_data)
            .context("missing NetworkAddress")?;
        let interface = properties
            .get("InterfaceIndex")
            .and_then(Value::as_unsigned_integer)
            .and_then(|v| v.try_into().ok())
            .unwrap_or(0);
        Some(network_address(bytes, interface)?)
    } else {
        None
    };
    let connection_type = match connection {
        "USB" => Connection::Usb,
        "Network" => Connection::Network(address.unwrap().ip),
        other => Connection::Unknown(other.into()),
    };
    Ok(Device {
        mux: UsbmuxdDevice {
            connection_type,
            udid: udid.into(),
            device_id: id.try_into()?,
        },
        address,
    })
}

pub(crate) fn parse(value: &Value) -> Result<Discovery> {
    let rows = value
        .as_dictionary()
        .and_then(|d| d.get("DeviceList"))
        .and_then(Value::as_array)
        .context("ListDevices response has no DeviceList array")?;
    let mut out = Discovery::default();
    for (i, row) in rows.iter().enumerate() {
        match device(row) {
            Ok(device) => out.devices.push(device),
            Err(e) => out.warnings.push(format!("DeviceList[{i}]: {e:#}")),
        }
    }
    if !out.warnings.is_empty() {
        events::record(
            "discovery",
            "warning",
            0,
            serde_json::json!({ "warnings": out.warnings }),
        );
    }
    if !rows.is_empty() && out.devices.is_empty() {
        bail!("no usable DeviceList entries: {}", out.warnings.join("; "));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_socket_addresses_retain_ipv6_interface_and_accept_both_os_layouts() {
        for family in [10, 23, 30] {
            let mut bytes = [0u8; 28];
            bytes[0] = family;
            bytes[8..24].copy_from_slice(&"fe80::1234".parse::<Ipv6Addr>().unwrap().octets());
            bytes[24..28].copy_from_slice(&12u32.to_ne_bytes());
            assert_eq!(
                network_address(&bytes, 0).unwrap().to_string(),
                "fe80::1234%12"
            );
            bytes[0] = 28;
            bytes[1] = family;
            assert_eq!(
                network_address(&bytes, 0).unwrap().to_string(),
                "fe80::1234%12"
            );
            bytes[24..28].fill(0);
            assert_eq!(
                network_address(&bytes, 7).unwrap().to_string(),
                "fe80::1234%7"
            );
            assert!(network_address(&bytes, 0).is_err());
        }
        for len in 0..28 {
            assert!(network_address(&vec![0; len], 0).is_err());
        }
    }

    #[test]
    fn malformed_rows_do_not_hide_a_valid_usb_device_and_are_not_silent() {
        let v = plist::to_value(&serde_json::json!({"DeviceList":[
            {"DeviceID":1,"Properties":{"SerialNumber":"PHONE","ConnectionType":"USB"}},
            {"DeviceID":2,"Properties":{"SerialNumber":"OTHER","ConnectionType":"Network"}}
        ]}))
        .unwrap();
        let out = parse(&v).unwrap();
        assert_eq!(out.devices.len(), 1);
        assert_eq!(out.devices[0].mux.connection_type, Connection::Usb);
        assert_eq!(out.warnings.len(), 1);
        assert!(
            parse(&plist::to_value(&serde_json::json!({"DeviceList":"bad"})).unwrap()).is_err()
        );
    }

    #[tokio::test]
    async fn malformed_frames_are_bounded_before_allocating_or_waiting_for_a_body() {
        for size in [0u32, 15, 16 * 1024 * 1024 + 1] {
            let (mut host, mut server) = tokio::io::duplex(4096);
            let (got, _) = tokio::join!(request(&mut host), async {
                let mut header = [0; 16];
                server.read_exact(&mut header).await.unwrap();
                let n = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
                server.read_exact(&mut vec![0; n - 16]).await.unwrap();
                let mut response = [0; 16];
                response[..4].copy_from_slice(&size.to_le_bytes());
                server.write_all(&response).await.unwrap();
            });
            assert!(got.is_err());
        }
    }
}
