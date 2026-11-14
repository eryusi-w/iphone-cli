//! iOS 17 미만 지원.
//!
//! iOS 17 이전에는 iph의 기본 경로가 쓰는 CoreDeviceProxy·RSD가 없어서
//! 모든 명령이 `InvalidService`로 실패한다. 대신 Xcode의 Instruments가 그
//! 버전에서 쓰는 경로를 쓴다: lockdown → 개발자 디스크 이미지(DDI) →
//! `com.apple.instruments.remoteserver`(DTX).
//!
//! 되는 것: 앱 목록(installation_proxy), 앱 실행(DVT process control),
//! 스크린샷(screenshotr).
//!
//! 안 되는 것: 화면 읽기와 터치·입력. iOS 17+의 접근성 서비스와 iOS 27의
//! displayservice가 없기 때문이다. 이 명령들은 조용히 실패하지 않고
//! 이유를 담은 오류를 돌려준다.

use std::time::{Duration, Instant};

use idevice::{
    IdeviceService,
    dvt::{process_control::ProcessControlClient, remote_server::RemoteServerClient},
    installation_proxy::InstallationProxyClient,
    lockdown::LockdownClient,
    mobile_image_mounter::ImageMounter,
    provider::IdeviceProvider,
    screenshotr::ScreenshotService,
};
use serde_json::{Value, json};

use crate::{
    device::{conn, wda}, fields, i18n::t, paths,
    reply::{Fail, Res, envelope},
};

/// 이 버전부터는 iph의 기본(CoreDevice/RSD) 경로가 동작한다.
pub const CORE_DEVICE_FROM: u64 = 17;

/// iOS 17 미만의 개발자 디스크 이미지(공개 저장소, doronz88/DeveloperDiskImage).
const DDI_BASE: &str =
    "https://raw.githubusercontent.com/doronz88/DeveloperDiskImage/main/DeveloperDiskImages";

/// 명령을 이 모듈이 직접 처리하는지: 폰과 대화해야 하는 명령 전부.
const PHONE_COMMANDS: &[&str] = &[
    "read", "find", "tap", "swipe", "scroll", "type", "key", "button", "launch", "apps",
    "screenshot", "batch", "script", "dnd",
];

/// 이 경로에서 실제로 되는 명령.
const SUPPORTED: &[&str] = &["apps", "launch", "screenshot"];

fn major_of(version: &str) -> Option<u64> {
    version.split('.').next()?.trim().parse().ok()
}

async fn version_of(provider: &dyn IdeviceProvider) -> Option<String> {
    let mut lock = tokio::time::timeout(Duration::from_secs(8), LockdownClient::connect(provider))
        .await
        .ok()?
        .ok()?;
    tokio::time::timeout(Duration::from_secs(5), lock.get_value(Some("ProductVersion"), None))
        .await
        .ok()?
        .ok()?
        .into_string()
}

fn err(code: &str, message: impl Into<String>) -> Fail {
    Fail::not_sent(code, message)
}

/// "15.8.8" → "15.8"; 이미지 저장소는 major.minor로 배포한다.
fn image_key(version: &str) -> String {
    let mut parts = version.split('.');
    match (parts.next(), parts.next()) {
        (Some(a), Some(b)) => format!("{a}.{b}"),
        (Some(a), None) => a.to_string(),
        _ => version.to_string(),
    }
}

async fn curl_to(url: &str, target: &std::path::Path) -> Result<(), String> {
    let status = tokio::process::Command::new("curl")
        .args(["-fsSL", "--retry", "2", "-o"])
        .arg(target)
        .arg(url)
        .status()
        .await
        .map_err(|e| format!("run curl: {e}"))?;
    if !status.success() {
        return Err(format!("{url}: curl said {status}"));
    }
    Ok(())
}

/// 개발자 디스크 이미지를 준비한다. 이미 마운트돼 있으면 아무것도 하지 않는다.
/// 없으면 캐시에서 읽고, 그것도 없으면 한 번 내려받는다(약 9 MB).
async fn ensure_ddi(provider: &dyn IdeviceProvider, version: &str) -> Res {
    let mut mounter = tokio::time::timeout(Duration::from_secs(15), ImageMounter::connect(provider))
        .await
        .map_err(|_| err("image_mounter_timeout", "the image mounter did not answer within 15 s"))?
        .map_err(|e| err("image_mounter_failed", format!("{e:?}")))?;

    // Already mounted is the common case; the lookup is cheap.
    if let Ok(Ok(_)) =
        tokio::time::timeout(Duration::from_secs(10), mounter.lookup_image("Developer")).await
    {
        return Ok(fields! { "ddi": "already mounted" });
    }

    let key = image_key(version);
    let dir = paths::ddi_cache().join("legacy").join(&key);
    let image_path = dir.join("DeveloperDiskImage.dmg");
    let signature_path = dir.join("DeveloperDiskImage.dmg.signature");

    if !image_path.exists() || !signature_path.exists() {
        std::fs::create_dir_all(&dir)
            .map_err(|e| err("ddi_cache_failed", format!("{}: {e}", dir.display())))?;
        let base = format!("{DDI_BASE}/{key}");
        for (name, target) in [
            ("DeveloperDiskImage.dmg", &image_path),
            ("DeveloperDiskImage.dmg.signature", &signature_path),
        ] {
            if target.exists() {
                continue;
            }
            let url = format!("{base}/{name}");
            let partial = target.with_extension("part");
            tokio::time::timeout(Duration::from_secs(180), curl_to(&url, &partial))
                .await
                .map_err(|_| err("ddi_download_timeout", format!("{url}: timed out")))?
                .map_err(|e| {
                    err("ddi_download_failed", e).hint(t(
                        "인터넷 연결을 확인하세요. iOS 15용 이미지는 공개 저장소에서 한 번만 받습니다.",
                        "Check the connection; the iOS 15 image is downloaded once from a public repository.",
                    ))
                })?;
            std::fs::rename(&partial, target)
                .map_err(|e| err("ddi_cache_failed", format!("{e}")))?;
        }
    }

    let image = std::fs::read(&image_path)
        .map_err(|e| err("ddi_cache_failed", format!("{}: {e}", image_path.display())))?;
    let signature = std::fs::read(&signature_path)
        .map_err(|e| err("ddi_cache_failed", format!("{}: {e}", signature_path.display())))?;

    tokio::time::timeout(Duration::from_secs(180), mounter.mount_developer(&image, signature))
        .await
        .map_err(|_| err("ddi_mount_timeout", "mounting the image timed out"))?
        .map_err(|e| {
            err("ddi_mount_failed", format!("{e:?}")).hint(t(
                "폰 잠금을 풀고 USB로 다시 연결한 뒤 시도하세요.",
                "Unlock the phone, reconnect by USB, and try again.",
            ))
        })?;

    Ok(fields! { "ddi": format!("mounted {key}") })
}

/// 앱 목록. iOS 17+의 appservice 대신 installation_proxy를 쓴다.
async fn cmd_apps(provider: &dyn IdeviceProvider) -> Res {
    let mut svc = tokio::time::timeout(Duration::from_secs(15), InstallationProxyClient::connect(provider))
        .await
        .map_err(|_| err("installation_proxy_timeout", "installation_proxy did not answer within 15 s"))?
        .map_err(|e| err("installation_proxy_failed", format!("{e:?}")))?;

    let apps = tokio::time::timeout(Duration::from_secs(30), svc.get_apps(Some("User"), None))
        .await
        .map_err(|_| err("installation_proxy_timeout", "the app list did not arrive within 30 s"))?
        .map_err(|e| err("installation_proxy_failed", format!("{e:?}")))?;

    let mut list: Vec<Value> = Vec::new();
    for (bundle, info) in apps {
        let Some(dict) = info.as_dictionary() else {
            continue;
        };
        let text = |key: &str| dict.get(key).and_then(|v| v.as_string()).map(|s| s.to_string());
        let name = text("CFBundleDisplayName")
            .or_else(|| text("CFBundleName"))
            .unwrap_or_else(|| bundle.clone());
        let first_party = text("ApplicationType").as_deref() == Some("System");
        list.push(json!({
            "bundle_id": bundle,
            "name": name,
            "version": text("CFBundleShortVersionString"),
            "first_party": first_party,
            "removable": dict.get("IsRemovable").and_then(|v| v.as_boolean()).unwrap_or(true),
        }));
    }
    list.sort_by(|a, b| a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or("")));
    Ok(fields! { "count": list.len(), "apps": list })
}

/// 이름이면 bundle id로 바꾼다. iOS 17+ 경로의 `resolve_bundle`과 같은 규칙.
async fn resolve_bundle(provider: &dyn IdeviceProvider, app: &str) -> Res<String> {
    if app.contains('.') && !app.contains(' ') {
        return Ok(app.to_string());
    }
    let wanted = app.to_lowercase();
    let mut svc = tokio::time::timeout(Duration::from_secs(15), InstallationProxyClient::connect(provider))
        .await
        .map_err(|_| err("installation_proxy_timeout", "installation_proxy did not answer within 15 s"))?
        .map_err(|e| err("installation_proxy_failed", format!("{e:?}")))?;
    let apps = tokio::time::timeout(Duration::from_secs(30), svc.get_apps(Some("User"), None))
        .await
        .map_err(|_| err("installation_proxy_timeout", "the app list did not arrive within 30 s"))?
        .map_err(|e| err("installation_proxy_failed", format!("{e:?}")))?;

    let mut hits: Vec<String> = Vec::new();
    for (bundle, info) in &apps {
        let Some(dict) = info.as_dictionary() else {
            continue;
        };
        let name = dict
            .get("CFBundleDisplayName")
            .or_else(|| dict.get("CFBundleName"))
            .and_then(|v| v.as_string())
            .unwrap_or("");
        if name.to_lowercase() == wanted {
            hits.push(bundle.clone());
        }
    }
    match hits.len() {
        1 => Ok(hits.remove(0)),
        0 => Err(
            err("app_not_found", format!("no installed app is named \"{app}\"")).hint(t(
                "`iph apps`로 이름과 bundle id를 확인하세요.",
                "See names and bundle ids with `iph apps`.",
            )),
        ),
        _ => Err(err("ambiguous_app", format!("{} apps are named \"{app}\"", hits.len()))
            .with("candidates", Value::Array(hits.into_iter().map(Value::String).collect()))),
    }
}

/// 앱 실행. Instruments의 프로세스 제어(DTX)로 띄운다. DDI가 마운트돼 있어야 한다.
async fn cmd_launch(provider: &dyn IdeviceProvider, args: &Value, version: &str) -> Res {
    let app = args["app"].as_str().unwrap_or_default();
    let restart = args["restart"].as_bool().unwrap_or(false);

    // 잠긴 폰에서는 Instruments가 PID를 주지 않는다. 미리 확인해 이유를 알려 준다.
    if conn::passcode_locked(provider).await == Some(true) {
        return Err(err("device_locked", "the phone is locked").hint(t(
            "폰 잠금을 풀고 다시 실행하세요. iOS 17 미만에서는 잠금 상태로 앱을 띄울 수 없습니다.",
            "Unlock the phone and try again; below iOS 17 an app cannot be launched while locked.",
        )));
    }

    let bundle = resolve_bundle(provider, app).await?;

    // Instruments(프로세스 제어)는 개발자 디스크 이미지에서 온다.
    ensure_ddi(provider, version).await?;

    let mut remote = tokio::time::timeout(Duration::from_secs(20), RemoteServerClient::connect(provider))
        .await
        .map_err(|_| err("instruments_timeout", "the instruments service did not answer within 20 s"))?
        .map_err(|e| {
            err(
                "instruments_unavailable",
                format!("cannot reach the instruments service on iOS {version}: {e:?}"),
            )
            .hint(t(
                "`iph doctor`로 개발자 디스크 이미지를 설치한 뒤 다시 시도하세요.",
                "Install the DeveloperDiskImage with `iph doctor`, then try again.",
            ))
        })?;

    let mut control = tokio::time::timeout(Duration::from_secs(15), ProcessControlClient::new(&mut remote))
        .await
        .map_err(|_| err("instruments_timeout", "process control did not answer within 15 s"))?
        .map_err(|e| err("instruments_failed", format!("{e:?}")))?;

    let began = Instant::now();
    let pid = tokio::time::timeout(
        Duration::from_secs(30),
        control.launch_app(bundle.clone(), None, None, false, restart),
    )
    .await
    .map_err(|_| err("launch_timeout", "no answer within 30 s").hint(t("폰 화면을 확인하세요.", "Look at the phone.")))?
    .map_err(|e| {
        // 여기서부터는 요청이 폰에 닿았을 수 있다.
        Fail::unknown("launch_failed", format!("{e:?}")).hint(t(
            "`iph apps`로 bundle id를 확인하고, 폰 잠금을 풀어 주세요.",
            "Check the bundle id with `iph apps`, and unlock the phone.",
        ))
    })?;

    Ok(fields! {
        "sent": true,
        // iOS 17 미만에는 접근성 서비스가 없어 전면 여부를 읽어 확인할 수 없다.
        "verified": false,
        "bundle_id": bundle,
        "pid": pid,
        "launch_ms": began.elapsed().as_millis() as u64,
        "note": t(
            "iOS 17 미만에서는 화면을 읽을 수 없어 전면 확인은 생략했습니다.",
            "On iOS below 17 the screen cannot be read, so the foreground check was skipped.",
        ),
        "ios_version": version,
    })
}

/// 스크린샷. 레거시 screenshotr 서비스(DDI 필요).
async fn cmd_screenshot(provider: &dyn IdeviceProvider, args: &Value, version: &str) -> Res {
    // screenshotr도 개발자 디스크 이미지가 있어야 뜬다.
    ensure_ddi(provider, version).await?;
    let mut svc = tokio::time::timeout(Duration::from_secs(20), ScreenshotService::connect(provider))
        .await
        .map_err(|_| err("screenshotr_timeout", "screenshotr did not answer within 20 s"))?
        .map_err(|e| {
            err("screenshotr_unavailable", format!("cannot reach screenshotr on iOS {version}: {e:?}")).hint(t(
                "`iph doctor`로 개발자 디스크 이미지를 설치한 뒤 다시 시도하세요.",
                "Install the DeveloperDiskImage with `iph doctor`, then try again.",
            ))
        })?;
    let png = tokio::time::timeout(Duration::from_secs(30), svc.take_screenshot())
        .await
        .map_err(|_| err("screenshotr_timeout", "the screenshot did not arrive within 30 s"))?
        .map_err(|e| err("screenshot_failed", format!("{e:?}")))?;

    let path = args["path"].as_str().unwrap_or("iph-screenshot.png");
    let path_buf = std::path::PathBuf::from(path);
    if let Some(dir) = path_buf.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)
            .map_err(|e| err("write_failed", format!("{}: {e}", dir.display())))?;
    }
    std::fs::write(&path_buf, &png).map_err(|e| err("write_failed", format!("{path}: {e}")))?;
    Ok(fields! { "path": path, "bytes": png.len(), "ios_version": version })
}

fn parse_point(text: &str) -> Option<(f64, f64)> {
    let (x, y) = text.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

fn wda_fail(error: anyhow::Error) -> Fail {
    Fail::unknown("wda_failed", format!("{error:#}")).hint(t(
        "WebDriverAgent 러너가 실행 중인지 `iph doctor`로 확인하세요.",
        "Check that the WebDriverAgent runner is running with `iph doctor`.",
    ))
}

/// WebDriverAgent가 떠 있으면 그쪽으로 손을 움직인다. 없으면 `None`.
async fn via_wda(provider: &dyn IdeviceProvider, cmd: &str, args: &Value) -> Option<Res> {
    if !matches!(cmd, "tap" | "type" | "swipe" | "button" | "read" | "find") {
        return None;
    }
    let mut client = wda::Wda::new(provider);
    if client.status().await.is_err() {
        return None;
    }
    let result = match cmd {
        "tap" => wda_tap(args, &mut client).await,
        "type" => wda_type(args, &mut client).await,
        "swipe" => wda_swipe(args, &mut client).await,
        "read" => wda_read(args, &mut client).await,
        "find" => wda_find(args, &mut client).await,
        _ => wda_button(args, &mut client).await,
    };
    // 러너에 세션이 쌓이지 않도록 닫는다(실패해도 결과에는 영향 없음).
    client.end().await;
    Some(result)
}

async fn wda_tap(args: &Value, client: &mut wda::Wda<'_>) -> Res {
    let target = args["target"].as_str().unwrap_or_default();
    let long_ms = args["long_ms"].as_u64();
    let double = args["double"].as_bool().unwrap_or(false);
    if (long_ms.is_some() || double) && parse_point(target).is_none() {
        // 요소를 찾아 그 중앙에서 제스처를 한다.
        let screen = client.screen().await.map_err(wda_fail)?;
        let element = match screen.elements.iter().find(|e| e.label.as_deref().is_some_and(|l| l == target || l.contains(target))) {
            Some(e) => e,
            None => {
                return Err(err("element_not_found", format!("no element matched \"{target}\""))
                    .hint(t("`iph read`로 라벨을 확인하세요.", "Check the label with `iph read`.")));
            }
        };
        let f = element.frame.ok_or_else(|| err("element_not_found", "element has no frame"))?;
        let (cx, cy) = (f.x + f.w / 2.0, f.y + f.h / 2.0);
        if double {
            client.double_tap(cx, cy).await.map_err(wda_fail)?;
        } else if let Some(ms) = long_ms {
            client.long_press(cx, cy, ms).await.map_err(wda_fail)?;
        }
        return Ok(fields! {
            "sent": true, "verified": false, "via": "wda", "target": target,
            "gesture": if double { "double_tap" } else { "long_press" },
        });
    }
    if let Some((x, y)) = parse_point(target) {
        if double {
            client.double_tap(x, y).await.map_err(wda_fail)?;
            return Ok(fields! { "sent": true, "verified": false, "via": "wda", "target": target, "gesture": "double_tap" });
        }
        if let Some(ms) = long_ms {
            client.long_press(x, y, ms).await.map_err(wda_fail)?;
            return Ok(fields! { "sent": true, "verified": false, "via": "wda", "target": target, "gesture": "long_press" });
        }
    }
    if let Some((x, y)) = parse_point(target) {
        client.tap_point(x, y).await.map_err(wda_fail)?;
        return Ok(fields! {
            "sent": true, "verified": false, "via": "wda",
            "target": target, "point": [x, y],
        });
    }
    client.tap_text(target).await.map_err(|e| {
        wda_fail(e).hint(t(
            "`iph doctor`로 WDA 상태를 확인하거나, 좌표(x,y)로 탭하세요.",
            "Check WDA with `iph doctor`, or tap by coordinate (x,y).",
        ))
    })?;
    Ok(fields! { "sent": true, "verified": false, "via": "wda", "target": target })
}

async fn wda_type(args: &Value, client: &mut wda::Wda<'_>) -> Res {
    let text = args["text"].as_str().unwrap_or_default().to_string();
    let submit = args["submit"].as_bool().unwrap_or(false);
    let mut payload = text.clone();
    if submit {
        payload.push('\n');
    }
    client.type_text(&payload).await.map_err(wda_fail)?;
    let mut out = fields! {
        "sent": true, "verified": false, "via": "wda",
        "text": text, "submitted": submit,
    };
    if args["clear"].as_bool().unwrap_or(false) {
        out.insert(
            "note".into(),
            json!(t(
                "WebDriverAgent 경로에서는 --clear를 지원하지 않습니다(기존 내용에 이어 씁니다).",
                "--clear is not supported over WebDriverAgent; the text is appended.",
            )),
        );
    }
    Ok(out)
}

async fn wda_swipe(args: &Value, client: &mut wda::Wda<'_>) -> Res {
    let (w, h) = client.window_size().await.map_err(wda_fail)?;
    let duration = args["duration_ms"].as_u64().unwrap_or(300);
    let (from, to) = if args["from"].as_array().is_some() {
        let from = args["from"].as_array().cloned().unwrap_or_default();
        let to = args["to"].as_array().cloned().unwrap_or_default();
        let num = |v: &[Value], i: usize| v.get(i).and_then(|x| x.as_f64());
        match (num(&from, 0), num(&from, 1), num(&to, 0), num(&to, 1)) {
            (Some(x1), Some(y1), Some(x2), Some(y2)) => ((x1, y1), (x2, y2)),
            _ => return Err(err("invalid_argument", "swipe needs from:[x,y] and to:[x,y]")),
        }
    } else {
        let direction = args["direction"].as_str().unwrap_or("up");
        match direction {
            "up" => ((w / 2.0, h * 0.75), (w / 2.0, h * 0.25)),
            "down" => ((w / 2.0, h * 0.25), (w / 2.0, h * 0.75)),
            "left" => ((w * 0.85, h / 2.0), (w * 0.15, h / 2.0)),
            "right" => ((w * 0.15, h / 2.0), (w * 0.85, h / 2.0)),
            other => {
                return Err(err("invalid_argument", format!(
                    "swipe takes up, down, left, right or x1,y1,x2,y2 (got {other})"
                )));
            }
        }
    };
    let sent = fields! {
        "sent": true, "verified": false, "via": "wda",
        "from": [from.0, from.1], "to": [to.0, to.1], "duration_ms": duration,
    };
    match client.swipe(from, to, duration).await {
        Ok(()) => Ok(sent),
        // WDA는 drag 응답을 제스처가 끝난 뒤에나 준다(때로 주지 않는다).
        // 화면이 이미 바뀌었을 수 있으므로 '전송됨'으로 알리고 확인은 화면으로 하게 한다.
        Err(e) if format!("{e:#}").contains("did not answer in time") => {
            let mut out = sent;
            out.insert("note".into(), json!(t(
                "WDA가 응답을 주지 않았지만 제스처는 전송됐습니다. `iph screenshot`으로 화면을 확인하세요.",
                "WDA sent no reply, but the gesture went out. Check the screen with `iph screenshot`.",
            )));
            Ok(out)
        }
        Err(e) => Err(wda_fail(e)),
    }
}

/// 읽기: WDA 페이지 소스를 iph 요소 형식으로 돌려준다.
async fn wda_read(args: &Value, client: &mut wda::Wda<'_>) -> Res {
    let mut screen = client.screen().await.map_err(|e| {
        wda_fail(e).hint(t(
            "WDA가 페이지 소스를 주지 않았습니다. 러너 상태를 `iph doctor`로 확인하세요.",
            "WDA gave no page source; check the runner with `iph doctor`.",
        ))
    })?;
    if !args["all"].as_bool().unwrap_or(false) {
        screen.elements.retain(|e| e.on_screen);
    }
    let count = screen.elements.len();
    Ok(fields! {
        "screen": { "width": screen.width, "height": screen.height },
        "apps": screen.apps,
        "count": count,
        "pass": "wda",
        "seq": 1,
        "elements": screen.elements,
        "via": "wda",
    })
}

/// 찾기: iph 선택자 문법을 그대로 WDA 트리에 적용한다.
async fn wda_find(args: &Value, client: &mut wda::Wda<'_>) -> Res {
    let sel = crate::selector::Selector::from_value(&args["selector"])
        .map_err(|e| Fail::invalid(e.to_string()))?;
    let screen = client.screen().await.map_err(wda_fail)?;
    let matches = sel.find(&screen);
    let elements: Vec<Value> = matches.iter().map(|e| json!(e)).collect();
    Ok(fields! {
        "selector": sel.describe(),
        "count": elements.len(),
        "elements": elements,
        "via": "wda",
    })
}

async fn wda_button(args: &Value, client: &mut wda::Wda<'_>) -> Res {
    let name = args["button"].as_str().unwrap_or("home");
    let mapped = match name {
        "home" => "home",
        "lock" | "power" => "lock",
        "volume_up" => "volumeUp",
        "volume_down" => "volumeDown",
        other => other,
    };
    client.press_button(mapped).await.map_err(wda_fail)?;
    Ok(fields! { "sent": true, "verified": false, "via": "wda", "button": name })
}

fn unsupported(cmd: &str, version: &str) -> Fail {
    err(
        "unsupported_ios_version",
        format!("`{cmd}` needs the iOS 17+ accessibility/CoreDevice services; this phone is iOS {version}"),
    )
    .hint(t(
        "iOS 17 미만에서는 `iph apps`, `iph launch`, `iph screenshot`이 동작하고, WebDriverAgent 러너가 떠 있으면 `tap`·`type`·`swipe`·`button`도 동작합니다(wda-build/README.md).",
        "On iOS below 17 `iph apps`, `iph launch` and `iph screenshot` work; with a WebDriverAgent runner running, `tap`, `type`, `swipe` and `button` work too (see wda-build/README.md).",
    ))
    .with("ios_version", json!(version))
    .with("supported", json!(SUPPORTED))
}

/// iOS 17~26에는 입력 서비스(displayservice, iOS 27)가 없어서 터치가 안 된다.
/// 이 구간은 WebDriverAgent 러너로만 손을 움직일 수 있다.
fn touch_needs_wda(major: u64) -> bool {
    (17..27).contains(&major)
}

/// iOS 17 미만 폰이면 이 명령을 여기서 처리하고 결과를 돌려준다.
/// iOS 17+ 이거나 이 모듈이 다루지 않는 명령이면 `None`.
pub async fn handle(udid: &str, cmd: &str, args: &Value, started: Instant) -> Option<Value> {
    if !PHONE_COMMANDS.contains(&cmd) {
        return None;
    }
    let route = conn::route(udid).await.ok()?;
    let version = version_of(&*route.provider).await?;
    let major = major_of(&version)?;
    if major >= CORE_DEVICE_FROM {
        return None;
    }
    let result: Res = match cmd {
        "apps" => cmd_apps(&*route.provider).await,
        "launch" => cmd_launch(&*route.provider, args, &version).await,
        "screenshot" => cmd_screenshot(&*route.provider, args, &version).await,
        other => match via_wda(&*route.provider, other, args).await {
            Some(res) => res,
            None => Err(unsupported(other, &version)),
        },
    };
    Some(envelope(result, started))
}

/// iOS 17~26의 입력 명령을 WDA로 보낸다. 그 밖의 버전은 `None`.
///
/// iOS 27+는 displayservice가 있어 원래 경로로 충분하고, iOS <17은
/// `handle`이 이미 처리한다. 여기서는 그 사이(iOS 18 등)만 맡는다.
pub async fn touch_fallback(
    udid: &str,
    cmd: &str,
    args: &Value,
    started: Instant,
) -> Option<Value> {
    let is_input = matches!(cmd, "tap" | "type" | "swipe" | "button");
    // iOS 17~26은 접근성 서비스 이름도 달라서(axAuditDaemon.remoteserver.shim.remote)
    // 원래 경로로는 읽기가 안 된다. WDA가 있으면 그쪽이 유일한 읽기 경로다.
    let is_read = matches!(cmd, "read" | "find");
    if !(is_input || is_read) {
        return None;
    }
    let route = conn::route(udid).await.ok()?;
    let version = version_of(&*route.provider).await?;
    let major = major_of(&version)?;
    if !touch_needs_wda(major) {
        return None;
    }
    match via_wda(&*route.provider, cmd, args).await {
        Some(res) => Some(envelope(res, started)),
        // WDA가 없으면 원래 경로를 그대로 시도한다. iOS 18처럼 입력 서비스가
        // 실제로 있는 버전도 있어서 여기서 단정하지 않는다.
        None => None,
    }

}

/// iOS 17 미만 폰이면 `iph doctor` 결과를 여기서 만든다.
pub async fn doctor(udid: &str, started: Instant) -> Option<Value> {
    let route = conn::route(udid).await.ok()?;
    let version = version_of(&*route.provider).await?;
    let major = major_of(&version)?;
    if major >= CORE_DEVICE_FROM {
        return None;
    }

    let ddi = ensure_ddi(&*route.provider, &version).await;
    let (ddi_status, ddi_detail, ready) = match ddi {
        Ok(f) => {
            let detail = f.get("ddi").and_then(|v| v.as_str()).unwrap_or("mounted").to_string();
            ("ok", detail, true)
        }
        Err(fail) => ("fail", fail.message, false),
    };

    let mut wda_check = wda::Wda::new(&*route.provider);
    let (wda_status, wda_detail) = match wda_check.status().await {
        Ok(_) => ("ok", "WebDriverAgent is answering on device port 8100".to_string()),
        Err(e) => ("skipped", format!("not running: {e:#}")),
    };
    let mut supported: Vec<&str> = SUPPORTED.to_vec();
    if wda_status == "ok" {
        supported.extend(["tap", "type", "swipe", "button", "read", "find"]);
    }

    let touch_ok = wda_status == "ok";
    let touch_detail = if touch_ok {
        "touch goes through the WebDriverAgent runner".to_string()
    } else {
        format!("iOS {version} has no input service; install a WebDriverAgent runner to enable tap/type/swipe")
    };
    let mut touch_check = json!({
        "check": "touch_input",
        "status": if touch_ok { "ok" } else { "skipped" },
        "required": false,
        "auto_fixed": false,
        "detail": touch_detail,
    });
    if !touch_ok {
        touch_check["user_action"] = json!(t(
            "터치 자동화에는 WebDriverAgent 러너가 필요합니다(wda-build/README.md).",
            "Touch automation needs a WebDriverAgent runner (see wda-build/README.md).",
        ));
    }

    let mut checks = vec![
        json!({ "check": "pairing", "status": "ok", "required": true, "auto_fixed": false }),
        json!({ "check": "connection", "status": "ok", "required": true, "auto_fixed": false,
                "detail": route.connection }),
        json!({ "check": "developer_disk_image", "status": ddi_status, "required": true,
                "auto_fixed": ddi_status == "ok", "detail": ddi_detail }),
        json!({ "check": "accessibility_tree", "status": "skipped", "required": false,
                "auto_fixed": false,
                "detail": format!("iOS {version} has no accessibility/CoreDevice service; `read` needs iOS 17+ or WDA") }),
        touch_check,
        json!({ "check": "webdriveragent", "status": wda_status, "required": false,
                "auto_fixed": false, "detail": wda_detail }),
    ];
    if !ready {
        checks[2]["user_action"] = json!(t(
            "인터넷 연결을 확인하세요. `iph doctor`가 개발자 디스크 이미지를 한 번 내려받습니다.",
            "Check the connection; `iph doctor` downloads the developer disk image once.",
        ));
    }

    let failing: Vec<Value> = checks
        .iter()
        .filter(|c| c["status"] == "fail")
        .map(|c| c["check"].clone())
        .collect();

    let known = ["read", "find", "tap", "swipe", "scroll", "type", "key", "button", "dnd", "script", "batch"];
    let unsupported: Vec<&str> = known
        .iter()
        .filter(|c| !supported.contains(c))
        .copied()
        .collect();

    let mut out = fields! {
        "ready": ready,
        "udid": udid,
        "connection": route.connection,
        "ios_version": version,
        "path": "legacy (lockdown + DeveloperDiskImage + Instruments)",
        "failing": failing,
        "checks": checks,
        "supported": supported,
        "unsupported": unsupported,
    };
    if !ready {
        out.insert("note".into(), json!(t(
            "개발자 디스크 이미지가 없어 앱 실행과 스크린샷만 막힙니다. `iph apps`는 이미 동작합니다.",
            "Without the developer disk image only launch and screenshot are blocked; `iph apps` already works.",
        )));
    }
    Some(envelope(Ok(out), started))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn major_parses() {
        assert_eq!(major_of("15.8.8"), Some(15));
        assert_eq!(major_of("17.0"), Some(17));
        assert_eq!(major_of("27.0.1"), Some(27));
        assert_eq!(major_of(""), None);
        assert_eq!(major_of("beta"), None);
    }

    #[test]
    fn touch_gate_covers_17_to_26_only() {
        assert!(!touch_needs_wda(16));
        assert!(touch_needs_wda(17));
        assert!(touch_needs_wda(18));
        assert!(touch_needs_wda(26));
        assert!(!touch_needs_wda(27));
    }

    #[test]
    fn core_device_threshold() {
        assert!(major_of("16.7.10").unwrap() < CORE_DEVICE_FROM);
        assert!(major_of("17.0").unwrap() >= CORE_DEVICE_FROM);
    }
}
