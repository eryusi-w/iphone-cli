//! WebDriverAgent ??????怨룹꽘??
//!
//! iOS 15???裕???⑥?뭵???낅슣??????ㅻ?????類λ룴??? ???⑸펲. ??ル‘逾???ル梨??XCTest ????
//! (WebDriverAgent)???굿? ??嶺뚮ㅄ維獄?? ?????쀣깷泥? ?リ옇?쀧뵳?8100 ????????????HTTP
//! API??usbmuxd ??⑤슡???熬곣뫖???嶺뚯쉳???嶺뚮씭???? ?곌랙?х뙴??熬곣뫁夷?筌뤾쑬裕????⑤벚????熬곣뫗?????⑸펲.
//!
//! ?뺢퀡??? ?筌뤾쑬??嶺뚮씭??キ?뤿Ь? ??븐슜爰???レ뒭筌??? ???⑸츩?????놁졑, ?????袁⑤뒆, ?뺢퀗??? ?????덈뺄,
//! ???꾩씩?源껎ｇ댆? ??븐뻼????袁ⓥ뵛(read)??iph????븐슜爰??筌먦끇六???????熬곣뫗異????쇳렩嶺뚯솘? ???낅츎??

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use idevice::{Idevice, ReadWrite, provider::IdeviceProvider};
use crate::ax::tree::{Element, Screen};
use crate::device::wda_source;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// WDA?띠럾? ?リ옇?쀧뵳??????影?ル츎 ????
pub const PORT: u16 = 8100;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Wda<'a> {
    provider: &'a dyn IdeviceProvider,
    session: Option<String>,
    ready: bool,
}

/// HTTP ??얜Ŧ堉???濡る룎. ??⑤객臾??袁⑤?獄?? ?곌랜梨뜻룇嶺????깅さ嶺???類ｋ펲.
struct Reply {
    status: u16,
    body: Vec<u8>,
}

fn header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// ???녹맠?띠럾? ??硫명뀬???곌랜梨뜻룇??????븐슦紐든춯?`Some`, ???꾩룇猷뉔뇡????濡?듆 `None`.
/// WDA??`Connection: close`????쒕샍????겶????猷????怨룹꽑 ?????EOF濚밸Ŧ?? ????さ嶺?????類ｋ펲.
fn try_parse(buf: &[u8]) -> Result<Option<Reply>> {
    let Some(end) = header_end(buf) else {
        return Ok(None);
    };
    let head = String::from_utf8_lossy(&buf[..end]).to_string();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow!("WDA sent a malformed status line: {status_line}"))?;

    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => content_length = value.trim().parse().ok(),
            "transfer-encoding" if value.trim().eq_ignore_ascii_case("chunked") => chunked = true,
            _ => {}
        }
    }

    let rest = &buf[end + 4..];
    if chunked {
        // 嶺뚮씭??嶺?"0\r\n\r\n"???????곌랜梨뜻룇????硫명뀊 ??
        let complete = rest.windows(5).any(|w| w == b"0\r\n\r\n") || rest.ends_with(b"0\r\n\r\n");
        if !complete {
            return Ok(None);
        }
        return Ok(Some(Reply { status, body: dechunk(rest)? }));
    }
    match content_length {
        Some(len) if rest.len() >= len => Ok(Some(Reply { status, body: rest[..len].to_vec() })),
        Some(_) => Ok(None),
        None => Ok(None),
    }
}

#[cfg(test)]
fn parse_reply(buf: Vec<u8>) -> Result<Reply> {
    try_parse(&buf)?.ok_or_else(|| anyhow!("WDA sent an incomplete HTTP reply"))
}

/// `Transfer-Encoding: chunked` ?곌랜梨뜻룇????怨룹꽑 ?釉먮듌???
fn dechunk(mut input: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let Some(eol) = input.windows(2).position(|w| w == b"\r\n") else {
            break;
        };
        let size_line = String::from_utf8_lossy(&input[..eol]).to_string();
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .with_context(|| format!("bad chunk size {size_hex:?}"))?;
        input = &input[eol + 2..];
        if size == 0 {
            break;
        }
        let take = size.min(input.len());
        out.extend_from_slice(&input[..take]);
        input = &input[take..];
        if input.starts_with(b"\r\n") {
            input = &input[2..];
        }
    }
    Ok(out)
}

impl<'a> Wda<'a> {
    pub fn new(provider: &'a dyn IdeviceProvider) -> Self {
        Self { provider, session: None, ready: false }
    }

    /// WDA?띠럾? ?影?ル츎 ??ㅻ걙???뿉??띠럾??????덈콦?? ?곌랜???? usbmuxd??濾곌쑨????リ옇?쀧뵳?8100 ????
    /// `IPH_WDA_ADDR=?筌뤾쑬裕?????????낅슣?????TCP ?낅슣?????븐뼚?붺뭐癒?괬?룹궠竊???????諭??.
    async fn stream(&self) -> Result<Box<dyn ReadWrite>> {
        if let Ok(addr) = std::env::var("IPH_WDA_ADDR") {
            if addr.trim().is_empty() {
                bail!("IPH_WDA_ADDR is empty");
            }
            let stream = tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(&addr))
                .await
                .map_err(|_| anyhow!("no answer from {addr}"))?
                .with_context(|| format!("connect to {addr}"))?;
            return Ok(Box::new(stream));
        }
        let device: Idevice = tokio::time::timeout(CONNECT_TIMEOUT, self.provider.connect(PORT))
            .await
            .map_err(|_| anyhow!("no answer from device port {PORT}"))?
            .with_context(|| format!("connect to device port {PORT}"))?;
        // Idevice?????猷???怨쀫닑亦????袁ぢ????????덈펲(get_socket??self???띠럾??筌롫챷???.
        device
            .get_socket()
            .ok_or_else(|| anyhow!("device connection closed before the request"))
    }

    async fn http(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Reply> {
        self.http_within(method, path, body, REPLY_TIMEOUT).await
    }

    /// ?묐떟???먮┛ ?붾뱶?ъ씤??`/source`)瑜??꾪빐 ?쒓컙???곕줈 以??
    async fn http_within(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        limit: Duration,
    ) -> Result<Reply> {
        let mut socket = self.stream().await?;

        let payload = match body {
            Some(v) => serde_json::to_vec(v)?,
            None => Vec::new(),
        };
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{PORT}\r\n\
             Accept: application/json\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        );
        tokio::time::timeout(limit, async {
            socket.write_all(head.as_bytes()).await?;
            socket.write_all(&payload).await?;
            socket.flush().await
        })
        .await
        .map_err(|_| anyhow!("WDA did not accept the request in time"))?
        .context("send to WDA")?;

        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        let reply = tokio::time::timeout(limit, async {
            loop {
                let n = socket.read(&mut chunk).await.context("read from WDA")?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > 16 * 1024 * 1024 {
                    bail!("WDA reply is too large");
                }
                match try_parse(&buf)? {
                    Some(reply) => return Ok(Some(reply)),
                    None => continue,
                }
            }
            Ok::<_, anyhow::Error>(try_parse(&buf)?)
        })
        .await
        .map_err(|_| anyhow!("WDA did not answer in time"))??;

        if buf.is_empty() {
            bail!("WDA closed the connection without an answer (runner not started?)");
        }
        reply.ok_or_else(|| anyhow!("WDA sent an incomplete HTTP reply"))
    }

    async fn json_request(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        self.json_request_within(method, path, body, REPLY_TIMEOUT).await
    }

    async fn json_request_within(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        limit: Duration,
    ) -> Result<Value> {
        let reply = self.http_within(method, path, body, limit).await?;
        let text = String::from_utf8_lossy(&reply.body).to_string();
        let value: Value = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).map_err(|e| anyhow!("WDA sent non-JSON: {e}: {text}"))?
        };
        if !(200..300).contains(&reply.status) {
            let message = value
                .get("value")
                .and_then(|v| v.get("message").or(Some(v)))
                .map(|v| v.to_string())
                .unwrap_or_else(|| text.clone());
            bail!("WDA answered {}: {message}", reply.status);
        }
        Ok(value)
    }

    /// WDA?띠럾? ???リ옇?쀧뵳????????덈뺄 繞벿살탳??우?? ?筌먦끉逾??類ｋ펲. ??怨몃さ嶺?`Err`.
    pub async fn status(&mut self) -> Result<Value> {
        let value = self.json_request("GET", "/status", None).await?;
        let state = value
            .get("value")
            .and_then(|v| v.get("state"))
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if !state.eq_ignore_ascii_case("success") && value.get("status").and_then(|s| s.as_i64()) != Some(0) {
            bail!("WDA is reachable but not ready: {value}");
        }
        self.ready = true;
        Ok(value)
    }

    /// ?筌뤾쑬???嶺뚮씭??キ?뤒?id??????삯쨹???????? ???깅さ嶺???亦??.
    pub async fn session(&mut self) -> Result<String> {
        if let Some(id) = &self.session {
            return Ok(id.clone());
        }
        if !self.ready {
            self.status().await?;
        }
        let body = json!({
            "capabilities": { "alwaysMatch": {
                "platformName": "iOS",
                "automationName": "XCUITest",
                "noReset": true,
                "shouldWaitForQuiescence": false,
            }}
        });
        let value = self.json_request("POST", "/session", Some(&body)).await?;
        let id = value
            .get("sessionId")
            .and_then(|v| v.as_str())
            .or_else(|| value.get("value").and_then(|v| v.get("sessionId")).and_then(|v| v.as_str()))
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("WDA created no session: {value}"))?;
        self.session = Some(id.clone());
        Ok(id)
    }

    /// ?筌뤾쑬??????ル츎????????筌뤾쑬????癰?逾좂춯?뼿 ???낆┣??. ???덉넮????쒕샍???類ｋ펲.
    pub async fn end(&mut self) {
        if let Some(id) = self.session.take() {
            let _ = self.http("DELETE", &format!("/session/{id}"), None).await;
        }
    }

    pub async fn window_size(&mut self) -> Result<(f64, f64)> {        let id = self.session().await?;
        let value = self.json_request("GET", &format!("/session/{id}/window/size"), None).await?;
        let w = value["value"]["width"].as_f64().unwrap_or(0.0);
        let h = value["value"]["height"].as_f64().unwrap_or(0.0);
        if w <= 0.0 || h <= 0.0 {
            bail!("WDA reported no window size: {value}");
        }
        Ok((w, h))
    }

    /// ??レ뒭筌??? WDA 16?? `/wda/tap/0`????怨룸쭑????? W3C actions???????
    pub async fn tap_point(&mut self, x: f64, y: f64) -> Result<()> {
        let id = self.session().await?;
        let body = pointer_actions(vec![
            json!({"type": "pointerMove", "duration": 0, "x": x, "y": y}),
            json!({"type": "pointerDown", "button": 0}),
            json!({"type": "pause", "duration": 80}),
            json!({"type": "pointerUp", "button": 0}),
        ]);
        self.json_request("POST", &format!("/session/{id}/actions"), Some(&body)).await?;
        Ok(())
    }

    /// ??怨뚮낵鸚???藥꿔꺃怡?뤆?繞???濡る룎?띠럾? exact?? ?띠룇?? 嶺???븐슜爰????????곌랜梨??
    ///
    /// ????キ?곗춹??덈펲 ?꾩룇猷뉔뇡??낅슣???locator?띠럾? ?????WDA 16?? `-ios predicate string`??
    /// 400??怨쀬Ŧ 濾곌쑨????類ｋ펲) ???????덈츎 ?롪퍒???嶺뚢뼰維?????類ｌ┣??類ｋ펲.
    /// ??륁뵠筌왖 ???뮞(`/source`)????뚮선 iph??`Screen`??곗쨮 獄쏅떽???
    pub async fn screen(&mut self) -> Result<Screen> {
        let id = self.session().await?;
        // WKWebView媛 ?욎씤 ?깆? ?뚯뒪媛 20珥??섍쾶 嫄몃┛??
        let value = self
            .json_request_within("GET", &format!("/session/{id}/source"), None, Duration::from_secs(120))
            .await?;
        let xml = value
            .get("value")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("WDA returned no page source: {value}"))?;
        wda_source::parse_source(xml)
    }

    /// ??곌볼嚥??遺용꺖??筌≪뼚釉?域???????꿰몴??袁ⓥ뀲?? WDA??element click?? ???源녿퓠??
    /// ?뚢뫂???瑗ョ몴????쑎 ?袁ⓓ???곕즲 ??깅선??? ??낅뮉 野껋럩??첎? 筌띾∥??
    pub async fn tap_text(&mut self, exact: &str) -> Result<()> {
        // 화면이 그려지는 중이면 소스가 일부만 온다. 몇 번 더 읽어 본다.
        let mut last: Option<(usize, usize)> = None;
        let mut element: Option<Element> = None;
        for attempt in 0..3 {
            let screen = self.screen().await?;
            if let Some(hit) = find_by_label(&screen, exact) {
                element = Some(hit.clone());
                break;
            }
            last = Some((screen.elements.len(), attempt + 1));
            if attempt < 2 {
                tokio::time::sleep(Duration::from_millis(800)).await;
            }
        }
        let element = element.ok_or_else(|| {
            let (count, tries) = last.unwrap_or((0, 0));
            anyhow!("no element matched \"{exact}\" ({count} elements read, {tries} tries)")
        })?;
        let frame = element.frame.ok_or_else(|| anyhow!("element \"{exact}\" has no frame"))?;
        if frame.w <= 0.0 || frame.h <= 0.0 {
            bail!("element \"{exact}\" has no size on screen");
        }
        self.tap_point(frame.x + frame.w / 2.0, frame.y + frame.h / 2.0)
            .await
    }

    pub async fn type_text(&mut self, text: &str) -> Result<()> {
        let id = self.session().await?;
        let body = json!({ "value": [text] });
        self.json_request("POST", &format!("/session/{id}/wda/keys"), Some(&body)).await?;
        Ok(())
    }

    pub async fn swipe(&mut self, from: (f64, f64), to: (f64, f64), duration_ms: u64) -> Result<()> {
        let id = self.session().await?;
        let body = pointer_actions(vec![
            json!({"type": "pointerMove", "duration": 0, "x": from.0, "y": from.1}),
            json!({"type": "pointerDown", "button": 0}),
            json!({"type": "pointerMove", "duration": duration_ms.max(50), "x": to.0, "y": to.1}),
            json!({"type": "pointerUp", "button": 0}),
        ]);
        self.json_request("POST", &format!("/session/{id}/actions"), Some(&body)).await?;
        Ok(())
    }

    pub async fn press_button(&mut self, name: &str) -> Result<()> {
        let id = self.session().await?;
        let body = json!({ "name": name });
        self.json_request("POST", &format!("/session/{id}/wda/pressButton"), Some(&body)).await?;
        Ok(())
    }

    pub async fn launch(&mut self, bundle: &str) -> Result<()> {
        let id = self.session().await?;
        let body = json!({ "bundleId": bundle });
        self.json_request("POST", &format!("/session/{id}/wda/apps/launch"), Some(&body)).await?;
        Ok(())
    }

    /// ??븐뻼??PNG. WDA??base64??????삯쨹????
    pub async fn screenshot_png(&mut self) -> Result<Vec<u8>> {
        let value = self.json_request("GET", "/screenshot", None).await?;
        let b64 = value
            .get("value")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("WDA returned no screenshot: {value}"))?;
        let bytes = base64_decode(b64)?;
        Ok(bytes)
    }
}

/// W3C pointer actions ??濡る룎(??⑥?뭵 ???????.
fn pointer_actions(steps: Vec<Value>) -> Value {
    json!({
        "actions": [{
            "type": "pointer",
            "id": "finger1",
            "parameters": { "pointerType": "touch" },
            "actions": steps,
        }]
    })
}

/// ??怨뚮낵???筌먐쇰꼪???띠룇?? ??븐슜爰?????怨뚮낵???????濡ル츎 ??븐슜爰????띠룆????띠룇?? ??븐슜爰???
fn find_by_label<'a>(screen: &'a Screen, needle: &str) -> Option<&'a Element> {
    let on_screen = || screen.elements.iter().filter(|e| e.on_screen);
    on_screen()
        .clone()
        .find(|e| e.label.as_deref() == Some(needle))
        .or_else(|| on_screen().find(|e| e.label.as_deref().is_some_and(|l| l.contains(needle))))
        .or_else(|| screen.elements.iter().find(|e| e.value.as_deref() == Some(needle)))
}
/// ?熬곣뫜????? base64 ??븐슧留????? ??怨뺤냱?? ???삳럠 ???깅뮔).
fn base64_decode(input: &str) -> Result<Vec<u8>> {
    fn value(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for c in input.bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        let v = value(c).ok_or_else(|| anyhow!("bad base64 character {:?}", c as char))?;
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    if out.is_empty() {
        bail!("empty base64 payload");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trip() {
        // "iph" = aXBo
        assert_eq!(base64_decode("aXBo").unwrap(), b"iph");
        assert_eq!(base64_decode("aXBo\n").unwrap(), b"iph");
        assert_eq!(base64_decode("aA==").unwrap(), b"h");
        assert!(base64_decode("!!!!").is_err());
    }

    #[test]
    fn parses_a_plain_reply() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\n{\"a\":1}".to_vec();
        let reply = parse_reply(raw).unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body, b"{\"a\":1}");
    }

    #[test]
    fn parses_a_chunked_reply() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n3\r\n:1}\r\n0\r\n\r\n".to_vec();
        let reply = parse_reply(raw).unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body, b"{\"a\":1}");
    }

    #[test]
    fn rejects_a_non_http_answer() {
        assert!(parse_reply(b"garbage".to_vec()).is_err());
    }
}
