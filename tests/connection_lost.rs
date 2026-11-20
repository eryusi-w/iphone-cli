//! A daemon accepted the request, then its reply was lost. The real CLI must
//! return unknown without replaying input, on both sockets and named pipes.
use iphone_cli::{daemon::Request, ipc};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::test]
async fn accepted_input_is_never_replayed_after_a_lost_reply() {
    let id = format!("TEST-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let base = if cfg!(unix) {
        std::path::PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    };
    let dir = base.join(format!("iph-lost-{id}"));
    std::fs::create_dir_all(&dir).unwrap();
    // This test is the only test in this process. Each child gets its own
    // environment; no user daemon or phone is involved.
    unsafe {
        std::env::set_var("IPH_STATE_DIR", &dir);
    }
    let mut listener = ipc::Listener::bind(&id).unwrap();
    let script = dir.join("input.js");
    std::fs::write(&script, "type('test')").unwrap();
    for args in [
        vec!["type", "test"],
        vec!["tap", "1,1"],
        vec!["script", script.to_str().unwrap()],
    ] {
        let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_iph"))
            .args(["--udid", &id])
            .args(&args)
            .env("IPH_STATE_DIR", &dir)
            .env("IPH_CONFIG_DIR", dir.join("config"))
            .env_remove("IPH_TRACE")
            .env_remove("IPH_LOG")
            .output();
        let accept = async {
            let stream = listener.accept().await.unwrap();
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).await.unwrap();
            let request: Request = serde_json::from_str(&line).unwrap();
            assert_eq!(request.cmd, args[0]);
            // Drop the stream with no reply, as if the daemon died after input.
        };
        let (output, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(child, accept)
        })
        .await
        .unwrap();
        let output = output.unwrap();
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(2), "{value}");
        assert_eq!(value["outcome"], "unknown", "{value}");
        assert_eq!(value["error"]["code"], "daemon_connection_lost");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "input was replayed"
        );
    }
    drop(listener);
    ipc::cleanup(&id);
    std::fs::remove_dir_all(dir).unwrap();
}
