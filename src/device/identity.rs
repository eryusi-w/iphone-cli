//! Paired, read-only identity checks shared by all Wi-Fi address sources.

use crate::reply::{Fail, Res};
use idevice::{IdeviceService, lockdown::LockdownClient, provider::IdeviceProvider};
use std::time::Duration;

pub(crate) struct Identity {
    pub udid: String,
    pub ios_version: Option<String>,
    pub model: Option<String>,
}

/// Read identity through a paired session, not an unauthenticated GetValue.
pub(crate) async fn identify(provider: &dyn IdeviceProvider) -> Res<Identity> {
    let result = tokio::time::timeout(Duration::from_secs(25), async {
        let mut phone = LockdownClient::connect(provider).await.map_err(|e| {
            Fail::not_sent(
                "wifi_connect_failed",
                format!("cannot reach the iPhone's lockdown service: {e:?}"),
            )
        })?;
        let pair = provider.get_pairing_file().await.map_err(|e| {
            Fail::not_sent(
                "not_paired",
                format!("cannot read the pairing record: {e:?}"),
            )
        })?;
        phone.start_session(&pair).await.map_err(|e| {
            Fail::not_sent(
                "wifi_session_failed",
                format!("cannot start a paired Wi-Fi session: {e:?}"),
            )
        })?;
        let identity = phone.get_value(Some("UniqueDeviceID"), None).await;
        let udid = identity
            .ok()
            .and_then(|v| v.into_string())
            .filter(|v| !v.is_empty());
        // Model/version are optional metadata; neither substitutes for identity.
        let version = phone
            .get_value(Some("ProductVersion"), None)
            .await
            .ok()
            .and_then(|v| v.into_string());
        let model = phone
            .get_value(Some("ProductType"), None)
            .await
            .ok()
            .and_then(|v| v.into_string());
        let _ = tokio::time::timeout(Duration::from_secs(2), phone.stop_session()).await;
        let udid = udid.ok_or_else(|| {
            Fail::not_sent(
                "wifi_identity_unavailable",
                "the paired session did not return a device UDID",
            )
        })?;
        Ok(Identity {
            udid,
            ios_version: version,
            model,
        })
    })
    .await;
    result.unwrap_or_else(|_| {
        Err(Fail::not_sent(
            "wifi_connect_failed",
            "direct Wi-Fi identity check timed out",
        ))
    })
}
