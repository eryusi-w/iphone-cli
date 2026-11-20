//! `iph devices`: connected iPhones and whether each is ready.

use std::time::Duration;

use idevice::{IdeviceService, lockdown::LockdownClient};
use serde_json::{Value, json};

use crate::{
    config::Config,
    device::{conn, discovery},
    fields,
    reply::{Fail, Res},
};

async fn lockdown_value(client: &mut LockdownClient, key: &str) -> Option<String> {
    tokio::time::timeout(Duration::from_secs(5), client.get_value(Some(key), None))
        .await
        .ok()?
        .ok()?
        .into_string()
}

/// Metadata from a route whose Wi-Fi identity was already checked.
async fn describe(route: &conn::Route) -> (Option<String>, Option<String>) {
    if let Some(identity) = &route.identity {
        return (identity.ios_version.clone(), identity.model.clone());
    }
    let connect = tokio::time::timeout(
        Duration::from_secs(8),
        LockdownClient::connect(&*route.provider),
    )
    .await;
    let Ok(Ok(mut client)) = connect else {
        return (None, None);
    };
    let version = lockdown_value(&mut client, "ProductVersion").await;
    let model = lockdown_value(&mut client, "ProductType").await;
    (version, model)
}

pub async fn cmd_devices(only: Option<String>) -> Res {
    let chosen = only
        .or_else(|| std::env::var("IPH_UDID").ok().filter(|u| !u.is_empty()))
        .or_else(|| Config::load().ok().and_then(|c| c.udid));
    let (found, discovery_error) = match discovery::read().await {
        Ok(found) => (found, None),
        Err(e) => (discovery::Discovery::default(), Some(format!("{e:#}"))),
    };
    let mut list = conn::summarize(found.devices.iter().map(|d| d.mux.clone()));
    let cached = chosen
        .as_ref()
        .filter(|u| !list.iter().any(|d| &d.udid == *u) && conn::recall_ip(u).is_some())
        .cloned();
    if let Some(udid) = &cached {
        list.push(conn::DeviceEntry {
            udid: udid.clone(),
            connection: "wifi".into(),
        });
    }
    if list.is_empty()
        && let Some(e) = &discovery_error
    {
        return Err(Fail::not_sent("usbmuxd_unreachable", e));
    }
    let mut mux = conn::mux_connect().await.ok();
    let mut out: Vec<Value> = Vec::new();
    for d in &list {
        let contact = chosen.as_ref().is_none_or(|c| *c == d.udid);
        let listed = cached.as_ref() != Some(&d.udid);
        let mut entry = json!({ "udid": d.udid, "connection": d.connection,
            "source": if listed { "usbmuxd" } else { "cached_wifi_address" },
            "transport": if d.connection == "wifi" { "tcp" } else { "usbmuxd" },
            "listed_by_usbmuxd": listed });
        if !listed {
            entry["ip"] = json!(conn::recall_ip(&d.udid).map(|ip| ip.to_string()));
        }
        if contact {
            let paired = match mux.as_mut() {
                Some(m) => tokio::time::timeout(Duration::from_secs(4), m.get_pair_record(&d.udid))
                    .await
                    .is_ok_and(|r| r.is_ok()),
                None => false,
            };
            let mut version = None;
            let mut model = None;
            if paired {
                let snapshot = match &discovery_error {
                    Some(e) => Err(anyhow::anyhow!(e.clone())),
                    None => Ok(found.clone()),
                };
                match conn::route_from(&d.udid, snapshot).await {
                    Ok(route) => {
                        entry["connection"] = json!(route.connection);
                        entry["transport"] = json!(if route.address.is_some() {
                            "tcp"
                        } else {
                            "usbmuxd"
                        });
                        entry["source"] = json!(route.source);
                        if let Some(ip) = route.address {
                            entry["ip"] = json!(ip.to_string());
                        }
                        (version, model) = describe(&route).await;
                    }
                    Err(e) => {
                        entry["connection_error"] = json!(format!("{e:#}"));
                    }
                }
            }
            entry["ios_version"] = json!(version);
            entry["model"] = json!(model);
            entry["paired"] = json!(paired);
            entry["ready"] = json!(paired && version.is_some());
            entry["state"] = json!(if !paired {
                "not paired"
            } else if version.is_none() {
                "unreachable"
            } else {
                "ready"
            });
        } else {
            entry["state"] = json!("not selected");
            entry["ready"] = json!(false);
        }
        out.push(entry);
    }
    Ok(
        fields! { "count": out.len(), "selected": chosen, "devices": out,
        "discovery_error": discovery_error, "discovery_warnings": found.warnings },
    )
}
