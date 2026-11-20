//! Register a paired phone's direct address when usbmuxd discovery is empty.

use std::{future::Future, path::Path};

use serde::Deserialize;

use crate::{
    client,
    device::{
        address::{self, WifiAddress},
        conn,
        identity::{self, Identity},
    },
    fields,
    i18n::t,
    paths,
    reply::{Fail, Res},
};

#[derive(Debug, Clone, clap::Args, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Args {
    /// The iPhone's IP address, shown in Settings > Wi-Fi > the network's info.
    #[arg(long)]
    pub ip: WifiAddress,
}

fn validate(udid: &str, _ip: WifiAddress) -> Res<()> {
    if !address::valid_udid(udid) {
        return Err(Fail::invalid(
            "UDID must contain only ASCII letters, digits and hyphens",
        ));
    }
    Ok(())
}

/// Persist only after both the paired session and reported identity succeeded.
async fn register_at(
    state: &Path,
    udid: &str,
    ip: WifiAddress,
    probe: impl Future<Output = Res<Identity>>,
) -> Res {
    validate(udid, ip)?;
    let identity = probe.await?;
    if !identity.udid.eq_ignore_ascii_case(udid) {
        return Err(Fail::not_sent(
            "wifi_device_mismatch",
            "the address belongs to a different iPhone",
        )
        .with("expected_udid", udid)
        .with("observed_udid", identity.udid));
    }
    address::save(state, udid, ip).map_err(|e| {
        Fail::not_sent(
            "connection_save_failed",
            format!("cannot save the verified address: {e}"),
        )
    })?;
    Ok(fields! {
        "udid": udid, "connection": "wifi", "source": "direct_ip",
        "ip": ip.to_string(), "paired": true, "udid_verified": true, "address_saved": true,
        "ios_version": identity.ios_version, "model": identity.model,
        "hint": t("주소를 저장했습니다. 이후 같은 UDID로 read·doctor·MCP·스크립트를 실행할 수 있습니다. 개발자 서비스 준비 상태는 doctor로 확인하세요.",
                  "Address saved. Use the same UDID for read, doctor, MCP and scripts. Check developer-service readiness with doctor.")
    })
}

pub async fn run(udid: &str, args: Args) -> Res {
    let probe = async {
        let provider = conn::direct_provider(udid, args.ip).await.map_err(|e| {
            Fail::not_sent(
                "not_paired",
                format!("cannot get this phone's pairing record: {e:#}"),
            )
        })?;
        identity::identify(&provider).await.map_err(|f| f.hint(t(
            "아이폰을 깨워 잠금을 풀고 IP 주소를 확인하세요. 처음 연결한다면 USB로 연결해 이 컴퓨터를 신뢰해야 합니다.",
            "Wake and unlock the iPhone, confirm its IP address, and pair by USB first if needed.",
        )))
    };
    let mut fields = register_at(&paths::state_dir(), udid, args.ip, probe).await?;
    // An existing daemon may still hold a tunnel to the previous address.
    fields.insert(
        "daemon_stopped".into(),
        client::stop_daemon(udid).await.is_some().into(),
    );
    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(udid: &str) -> Identity {
        Identity {
            udid: udid.into(),
            ios_version: Some("27.2.1".into()),
            model: Some("iPhone14,2".into()),
        }
    }

    #[tokio::test]
    async fn only_a_matching_paired_probe_replaces_the_saved_address() {
        let dir = std::env::temp_dir().join(format!("iph-direct-{}", uuid::Uuid::new_v4()));
        let udid = "TEST-PHONE";
        let old: WifiAddress = "192.0.2.1".parse().unwrap();
        let new: WifiAddress = "192.0.2.2".parse().unwrap();
        let saved = dir.join(format!("{udid}.addr"));
        let v = register_at(&dir, udid, old, async { Ok(identity(udid)) })
            .await
            .unwrap();
        assert_eq!(v["address_saved"], true);
        assert_eq!(std::fs::read_to_string(&saved).unwrap(), old.to_string());

        for result in [
            Err(Fail::not_sent("wifi_session_failed", "rejected")),
            Err(Fail::not_sent("wifi_identity_unavailable", "unreadable")),
            Ok(identity("OTHER-PHONE")),
        ] {
            assert!(
                register_at(&dir, udid, new, async { result })
                    .await
                    .is_err()
            );
            assert_eq!(std::fs::read_to_string(&saved).unwrap(), old.to_string());
        }
        register_at(&dir, udid, new, async { Ok(identity(udid)) })
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&saved).unwrap(), new.to_string());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn invalid_target_is_rejected_before_probing_or_creating_files() {
        let dir = std::env::temp_dir().join(format!("iph-direct-{}", uuid::Uuid::new_v4()));
        let result = register_at(&dir, "../escape", "192.0.2.1".parse().unwrap(), async {
            panic!("must not probe")
        })
        .await;
        assert_eq!(result.unwrap_err().code, "invalid_argument");
        assert!(!dir.exists());
    }

    #[tokio::test]
    async fn failed_cache_commit_preserves_existing_data_and_removes_the_temporary_file() {
        let dir = std::env::temp_dir().join(format!("iph-direct-{}", uuid::Uuid::new_v4()));
        let destination = dir.join("TEST-PHONE.addr");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("keep"), "previous data").unwrap();
        let result = register_at(&dir, "TEST-PHONE", "192.0.2.1".parse().unwrap(), async {
            Ok(identity("TEST-PHONE"))
        })
        .await;
        assert_eq!(result.unwrap_err().code, "connection_save_failed");
        assert_eq!(
            std::fs::read_to_string(destination.join("keep")).unwrap(),
            "previous data"
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
