//! WebDriverAgent의 페이지 소스(`/source`, XML)를 iph의 `Screen`으로 바꾼다.
//!
//! WDA는 XCTest가 보는 접근성 요소를 XML로 준다. iph의 기본 경로(iOS 17+ RSD)는
//! 접근성 서비스에서 같은 정보를 받지만, iOS 15에서는 그 서비스가 없어 WDA가
//! 유일한 출처다. 그래서 `iph read`/`find`가 이 변환을 쓴다.

use std::collections::HashMap;

use anyhow::{Result, bail};
use quick_xml::Reader;
use quick_xml::events::Event;

use crate::ax::tree::{AppInfo, Element, Rect, Screen};

/// XCUIElementType* → iph의 역할 이름.
fn role_of(xcui_type: &str) -> &'static str {
    let t = xcui_type.trim_start_matches("XCUIElementType");
    match t {
        "Application" => "application",
        "Button" => "button",
        "Switch" | "Toggle" => "switch",
        "TextField" | "SecureTextField" => "text_field",
        "SearchField" => "search_field",
        "StaticText" => "text",
        "Image" => "image",
        "Link" => "link",
        "Cell" => "other",
        "NavigationBar" | "TabBar" => "other",
        "Slider" | "Stepper" | "PageIndicator" => "adjustable",
        _ => "other",
    }
}


/// `{{0, 0}, {414, 736}}` 형태의 frame 문자열에서 사각형을 읽는다.
fn parse_frame(text: &str) -> Option<Rect> {
    let nums: Vec<f64> = text
        .split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    if nums.len() != 4 {
        return None;
    }
    Some(Rect { x: nums[0], y: nums[1], w: nums[2], h: nums[3] })
}

/// WDA 소스 XML을 요소 목록으로 바꾼다.
pub fn parse_source(xml: &str) -> Result<Screen> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut elements: Vec<Element> = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut apps: Vec<AppInfo> = Vec::new();
    let mut width = 0.0;
    let mut height = 0.0;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let raw_name = e.name().as_ref().to_string();
                let mut attrs: HashMap<String, String> = HashMap::new();
                for attr in e.attributes().flatten() {
                    let key = attr.key.as_ref().to_string();
                    let value = attr
                        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                        .map(|c| c.into_owned())
                        .unwrap_or_else(|_| attr.value.to_string());
                    attrs.insert(key, value);
                }
                // WDA는 최상위에 AppiumAUT 하나를 둔다. 요소가 아니다.
                if raw_name == "AppiumAUT" {
                    continue;
                }

                let xcui_type = attrs
                    .get("type")
                    .cloned()
                    .unwrap_or_else(|| raw_name.clone());
                let role = role_of(&xcui_type);
                let label = attrs.get("label").filter(|s| !s.is_empty()).cloned();
                let name = attrs.get("name").filter(|s| !s.is_empty()).cloned();
                let value = attrs.get("value").filter(|s| !s.is_empty()).cloned();
                let identifier = name.clone();

                let frame = attrs
                    .get("frame")
                    .and_then(|f| parse_frame(f))
                    .or_else(|| {
                        let x = attrs.get("x")?.parse::<f64>().ok()?;
                        let y = attrs.get("y")?.parse::<f64>().ok()?;
                        let w = attrs.get("width")?.parse::<f64>().ok()?;
                        let h = attrs.get("height")?.parse::<f64>().ok()?;
                        Some(Rect { x, y, w, h })
                    });

                if role == "application" {
                    if let (Some(f), true, true) = (frame, width == 0.0, height == 0.0) {
                        width = f.w;
                        height = f.h;
                    }
                    let app_name = label.clone().or(name.clone());
                    apps.push(AppInfo { pid: 0, name: app_name, bundle_id: None });
                }

                // visible 속성이 없으면 화면에 있다고 본다(WDA 버전마다 다르다).
                let visible = attrs.get("visible").map(|v| v == "true").unwrap_or(true);
                let enabled = attrs.get("enabled").map(|v| v == "true").unwrap_or(true);
                let focused = attrs.get("focused").map(|v| v == "true").unwrap_or(false)
                    || attrs.get("hasFocus").map(|v| v == "true").unwrap_or(false);
                let id = format!("{:04x}", elements.len());
                let parent = stack.last().cloned();
                elements.push(Element {
                    id: id.clone(),
                    role: role.to_string(),
                    label: label.clone().or_else(|| name.clone()),
                    value,
                    identifier,
                    hint: None,
                    frame,
                    traits: attrs
                        .get("type")
                        .map(|t| vec![t.clone()])
                        .unwrap_or_default(),
                    enabled,
                    selected: attrs.get("selected").map(|v| v == "true").unwrap_or(false),
                    focused,
                    parent,
                    depth: stack.len() as u32,
                    pid: 0,
                    on_screen: visible,
                    raw: None,
                    key: None,
                });

                stack.push(id);
            }
            Ok(Event::End(_)) => {
                stack.pop();
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => bail!("WDA source is not valid XML: {e}"),
        }
    }

    if elements.is_empty() {
        bail!("WDA source had no elements");
    }
    // Application이 크기를 주지 않았으면 화면 크기를 요소에서 추정한다.
    if width == 0.0 || height == 0.0 {
        let widest = elements.iter().filter_map(|e| e.frame).fold((0.0f64, 0.0f64), |acc, f| {
            (acc.0.max(f.x + f.w), acc.1.max(f.y + f.h))
        });
        width = widest.0;
        height = widest.1;
    }

    Ok(Screen { width, height, apps, elements })
}



#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<AppiumAUT>
  <XCUIElementTypeApplication type="XCUIElementTypeApplication" name="금캐기" label="금캐기" enabled="true" visible="true" x="0" y="0" width="414" height="736">
    <XCUIElementTypeWindow type="XCUIElementTypeWindow" enabled="true" visible="true" x="0" y="0" width="414" height="736">
      <XCUIElementTypeButton type="XCUIElementTypeButton" name="채굴기 내역" label="채굴기 내역" enabled="true" visible="true" x="20" y="400" width="180" height="40"/>
      <XCUIElementTypeStaticText type="XCUIElementTypeStaticText" name="0.001 g" label="0.001 g" value="0.001 g" enabled="true" visible="true" x="20" y="40" width="120" height="20"/>
    </XCUIElementTypeWindow>
  </XCUIElementTypeApplication>
</AppiumAUT>"#;

    #[test]
    fn parses_the_tree() {
        let screen = parse_source(SAMPLE).unwrap();
        assert_eq!(screen.width, 414.0);
        assert_eq!(screen.height, 736.0);
        assert_eq!(screen.apps.len(), 1);
        assert_eq!(screen.apps[0].name.as_deref(), Some("금캐기"));
        // application + window + button + text
        assert_eq!(screen.elements.len(), 4);
        let button = screen.elements.iter().find(|e| e.role == "button").unwrap();
        assert_eq!(button.label.as_deref(), Some("채굴기 내역"));
        let frame = button.frame.unwrap();
        assert_eq!((frame.x, frame.y, frame.w, frame.h), (20.0, 400.0, 180.0, 40.0));
        assert_eq!(button.depth, 2);
        assert!(screen.elements.iter().any(|e| e.role == "text"));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_source("<not xml").is_err());
        assert!(parse_source("<AppiumAUT></AppiumAUT>").is_err());
    }

    #[test]
    fn reads_a_frame_string() {
        let rect = parse_frame("{{0, 0}, {414, 736}}").unwrap();
        assert_eq!((rect.x, rect.y, rect.w, rect.h), (0.0, 0.0, 414.0, 736.0));
    }
}
