//! The real CLI with an empty Apple discovery list and no pairing record.
//! No phone, host service, or developer tunnel is contacted.
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use serde_json::{Value, json};

struct Fixture {
    dir: PathBuf,
    udid: String,
    address: String,
    done: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Fixture {
    fn new() -> Self {
        Self::with_listing(plist::to_value(&json!({"DeviceList": []})).unwrap())
    }

    fn with_listing(mut listing: plist::Value) -> Self {
        let base = if cfg!(unix) {
            PathBuf::from("/tmp")
        } else {
            std::env::temp_dir()
        };
        let dir = base.join(format!(
            "iph-route-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ));
        let udid = format!("TEST-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        if let Some(rows) = listing
            .as_dictionary_mut()
            .and_then(|d| d.get_mut("DeviceList"))
            .and_then(plist::Value::as_array_mut)
        {
            for row in rows {
                row.as_dictionary_mut()
                    .unwrap()
                    .get_mut("Properties")
                    .unwrap()
                    .as_dictionary_mut()
                    .unwrap()
                    .insert("SerialNumber".into(), udid.clone().into());
            }
        }
        std::fs::create_dir_all(dir.join("state")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        listener.set_nonblocking(true).unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let shutdown = done.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let server = thread::spawn(move || {
            while !shutdown.load(Ordering::Relaxed) {
                if let Ok((stream, _)) = listener.accept() {
                    let _ = serve(stream, &listing, &recorded);
                } else {
                    thread::sleep(Duration::from_millis(2));
                }
            }
        });
        Self {
            dir,
            udid,
            address,
            done,
            server: Some(server),
            requests,
        }
    }

    fn cache(&self) -> PathBuf {
        self.dir.join("state").join(format!("{}.addr", self.udid))
    }

    fn run(&self, args: &[&str]) -> (i32, Value) {
        self.run_prefer(args, "usb")
    }

    fn run_prefer(&self, args: &[&str], prefer: &str) -> (i32, Value) {
        let out = Command::new(env!("CARGO_BIN_EXE_iph"))
            .args(args.iter().map(|a| {
                if *a == "TEST-PHONE" {
                    self.udid.as_str()
                } else {
                    a
                }
            }))
            .env("IPH_STATE_DIR", self.dir.join("state"))
            .env("IPH_CONFIG_DIR", self.dir.join("config"))
            .env("USBMUXD_SOCKET_ADDRESS", &self.address)
            .env("IPH_PREFER", prefer)
            .env_remove("IPH_UDID")
            .env_remove("IPH_TRACE")
            .env_remove("IPH_LOG")
            .output()
            .unwrap();
        let value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stderr)));
        (out.status.code().unwrap(), value)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.run(&["--udid", "TEST-PHONE", "stop"]);
        self.done.store(true, Ordering::Relaxed);
        self.server.take().unwrap().join().unwrap();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn serve(
    mut stream: TcpStream,
    listing: &plist::Value,
    requests: &Mutex<Vec<String>>,
) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    loop {
        let mut header = [0; 16];
        stream.read_exact(&mut header)?;
        let n = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
        assert!((16..1024 * 1024).contains(&n));
        let mut body = vec![0; n - 16];
        stream.read_exact(&mut body)?;
        let request = plist::Value::from_reader(std::io::Cursor::new(body)).unwrap();
        let kind = request
            .as_dictionary()
            .unwrap()
            .get("MessageType")
            .unwrap()
            .as_string()
            .unwrap();
        requests.lock().unwrap().push(kind.to_string());
        // A cached address is never enough to authenticate a target: no
        // pairing record is returned, and no Connect request is allowed.
        let response = match kind {
            "ListDevices" => listing.clone(),
            "ReadPairRecord" => {
                plist::to_value(&json!({"MessageType": "Result", "Number": 2})).unwrap()
            }
            "Connect" => plist::to_value(&json!({"MessageType": "Result", "Number": 3})).unwrap(),
            other => panic!("unexpected mux request: {other}"),
        };
        let mut payload = Vec::new();
        response.to_writer_xml(&mut payload).unwrap();
        stream.write_all(&((payload.len() + 16) as u32).to_le_bytes())?;
        stream.write_all(&header[4..])?;
        stream.write_all(&payload)?;
    }
}

#[test]
fn selected_cached_candidate_is_reported_without_claiming_apple_discovery_or_readiness() {
    let f = Fixture::new();
    let (_, empty) = f.run(&["--udid", "TEST-PHONE", "devices"]);
    assert_eq!(empty["count"], 0);
    std::fs::write(f.cache(), "192.0.2.1").unwrap();
    std::fs::write(f.dir.join("state/OTHER-PHONE.addr"), "192.0.2.2").unwrap();
    let (code, listed) = f.run(&["--udid", "TEST-PHONE", "devices"]);
    assert_eq!(code, 0);
    assert_eq!(listed["count"], 1);
    let d = &listed["devices"][0];
    assert_eq!(d["udid"], f.udid);
    assert_eq!(d["source"], "cached_wifi_address");
    assert_eq!(d["listed_by_usbmuxd"], false);
    assert_eq!(d["ready"], false);
    assert_eq!(d["paired"], false);
    // No target selected: unrelated/stale caches are not contacted or
    // silently selected as the user's phone.
    let (_, unselected) = f.run(&["devices"]);
    assert_eq!(unselected["count"], 0);
}

#[test]
fn connect_requires_pairing_and_keeps_the_previous_address_on_failure() {
    let f = Fixture::new();
    let saved = f.cache();
    std::fs::write(&saved, "192.0.2.1").unwrap();
    let (code, result) = f.run(&["--udid", "TEST-PHONE", "connect", "--ip", "192.0.2.2"]);
    assert_eq!(code, 1);
    assert_eq!(result["outcome"], "not_sent");
    assert_eq!(result["error"]["code"], "not_paired");
    assert_eq!(std::fs::read_to_string(saved).unwrap(), "192.0.2.1");
}

fn listing(connection: &str) -> plist::Value {
    let mut value = plist::to_value(&json!({"DeviceList": [{"DeviceID": 1,
        "Properties": {"SerialNumber": "TEST-PHONE", "ConnectionType": connection}}]}))
    .unwrap();
    if connection == "Network" {
        let mut address = vec![0u8; 16];
        address[0] = 16;
        address[1] = 2;
        address[4..8].copy_from_slice(&[192, 0, 2, 1]);
        value
            .as_dictionary_mut()
            .unwrap()
            .get_mut("DeviceList")
            .unwrap()
            .as_array_mut()
            .unwrap()[0]
            .as_dictionary_mut()
            .unwrap()
            .get_mut("Properties")
            .unwrap()
            .as_dictionary_mut()
            .unwrap()
            .insert("NetworkAddress".into(), plist::Value::Data(address));
    }
    value
}

#[test]
fn discovered_wifi_uses_direct_tcp_and_never_mux_connect() {
    let f = Fixture::with_listing(listing("Network"));
    let (code, result) = f.run(&["--udid", "TEST-PHONE", "read"]);
    assert_eq!(
        (code, result["outcome"].as_str()),
        (1, Some("not_sent")),
        "{result}"
    );
    let requests = f.requests.lock().unwrap();
    assert!(
        requests.iter().any(|r| r == "ReadPairRecord"),
        "{requests:?}"
    );
    assert!(
        !requests.iter().any(|r| r == "Connect"),
        "Wi-Fi must never tunnel through usbmuxd: {requests:?}"
    );
    assert!(
        !f.cache().exists(),
        "an unauthenticated address must not be saved"
    );
}

#[test]
fn failed_list_query_does_not_block_the_selected_cache_or_hide_discovery_errors() {
    let f = Fixture::with_listing(plist::to_value(&json!({"Number": 1})).unwrap());
    std::fs::write(f.cache(), "192.0.2.1").unwrap();
    let (_, devices) = f.run(&["--udid", "TEST-PHONE", "devices"]);
    assert_eq!(devices["count"], 1, "{devices}");
    assert!(
        devices["discovery_error"]
            .as_str()
            .unwrap()
            .contains("DeviceList")
    );
    assert_eq!(devices["devices"][0]["listed_by_usbmuxd"], false);
    f.requests.lock().unwrap().clear();
    let (code, result) = f.run(&["--udid", "TEST-PHONE", "read"]);
    assert_eq!(
        (code, result["outcome"].as_str()),
        (1, Some("not_sent")),
        "{result}"
    );
    assert!(
        result["error"]["message"]
            .as_str()
            .unwrap()
            .contains("pairing record"),
        "the cached candidate must be attempted: {result}"
    );
    assert!(
        f.requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r == "ReadPairRecord")
    );
    assert_eq!(std::fs::read_to_string(f.cache()).unwrap(), "192.0.2.1");
}

#[test]
fn usb_stays_first_and_only_connection_setup_falls_back_to_cached_wifi() {
    let f = Fixture::with_listing(listing("USB"));
    std::fs::write(f.cache(), "192.0.2.1").unwrap();
    let (code, result) = f.run(&["--udid", "TEST-PHONE", "read"]);
    assert_eq!(
        (code, result["outcome"].as_str()),
        (1, Some("not_sent")),
        "{result}"
    );
    let requests = f.requests.lock().unwrap().clone();
    let usb = requests.iter().position(|r| r == "Connect").unwrap();
    let wifi = requests.iter().position(|r| r == "ReadPairRecord").unwrap();
    assert!(usb < wifi, "{requests:?}");
    f.run(&["--udid", "TEST-PHONE", "stop"]);
    f.requests.lock().unwrap().clear();
    f.run_prefer(&["--udid", "TEST-PHONE", "read"], "wifi");
    let requests = f.requests.lock().unwrap();
    assert!(
        requests.iter().position(|r| r == "ReadPairRecord").unwrap()
            < requests.iter().position(|r| r == "Connect").unwrap(),
        "{requests:?}"
    );
}
