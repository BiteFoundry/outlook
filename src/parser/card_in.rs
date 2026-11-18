//! Adaptive Card and O365 Connector Card JSON parser -> `RichBlock::Card`.

use serde_json::Value;

use crate::models::{AttachmentInfo, RichBlock};

pub const ADAPTIVE_CARD_CONTENT_TYPE: &str = "application/vnd.microsoft.card.adaptive";
pub const CONNECTOR_CARD_CONTENT_TYPE: &str = "application/vnd.microsoft.teams.card.o365connector";

pub fn is_card_content_type(content_type: &str) -> bool {
    let ct = content_type.trim().to_ascii_lowercase();
    ct == ADAPTIVE_CARD_CONTENT_TYPE || ct == CONNECTOR_CARD_CONTENT_TYPE
}

pub fn parse_card_attachment(att: &AttachmentInfo) -> Option<RichBlock> {
    let raw = att.content_json.as_deref()?.trim();
    if raw.is_empty() {
        return None;
    }
    let val: Value = serde_json::from_str(raw).ok()?;
    parse_card_value(&val, att.name.as_deref())
}

pub fn parse_card_value(val: &Value, fallback_name: Option<&str>) -> Option<RichBlock> {
    let obj = val.as_object()?;
    let is_connector = obj.contains_key("sections")
        || obj.contains_key("potentialAction")
        || obj
            .get("@type")
            .and_then(Value::as_str)
            .map(|t| t.eq_ignore_ascii_case("MessageCard"))
            .unwrap_or(false);

    if is_connector && !obj.contains_key("body") {
        return Some(parse_o365_connector(obj, fallback_name));
    }

    Some(parse_adaptive_card(obj, fallback_name))
}

fn parse_adaptive_card(obj: &serde_json::Map<String, Value>, fallback_name: Option<&str>) -> RichBlock {
    let mut title: Option<String> = obj
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let mut subtitle: Option<String> = None;
    let mut facts: Vec<(String, String)> = Vec::new();
    let mut body_lines: Vec<String> = Vec::new();
    let mut actions: Vec<(String, String)> = Vec::new();

    if let Some(body) = obj.get("body").and_then(Value::as_array) {
        for elem in body {
            collect_adaptive_element(
                elem,
                &mut title,
                &mut subtitle,
                &mut facts,
                &mut body_lines,
                &mut actions,
            );
        }
    }

    if let Some(card_actions) = obj.get("actions").and_then(Value::as_array) {
        collect_adaptive_actions(card_actions, &mut actions);
    }

    if title.is_none() {
        title = fallback_name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    }

    RichBlock::Card {
        title,
        subtitle,
        facts,
        body_lines,
        actions,
    }
}

fn collect_adaptive_element(
    elem: &Value,
    title: &mut Option<String>,
    subtitle: &mut Option<String>,
    facts: &mut Vec<(String, String)>,
    body_lines: &mut Vec<String>,
    actions: &mut Vec<(String, String)>,
) {
    let Some(obj) = elem.as_object() else {
        return;
    };
    let elem_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
    match elem_type {
        "TextBlock" => {
            let text = obj
                .get("text")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            if text.is_empty() {
                return;
            }
            let size = obj.get("size").and_then(Value::as_str).unwrap_or("");
            let weight = obj.get("weight").and_then(Value::as_str).unwrap_or("");
            let is_subtle = obj.get("isSubtle").and_then(Value::as_bool).unwrap_or(false);
            let is_heading = matches!(size, "Large" | "ExtraLarge" | "Medium")
                || weight.eq_ignore_ascii_case("Bolder");

            if title.is_none() && is_heading {
                *title = Some(text.to_string());
            } else if subtitle.is_none() && title.is_some() && is_subtle && body_lines.is_empty() {
                *subtitle = Some(text.to_string());
            } else {
                for line in text.lines() {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        body_lines.push(trimmed.to_string());
                    }
                }
            }
        }
        "FactSet" => {
            if let Some(arr) = obj.get("facts").and_then(Value::as_array) {
                for fact in arr {
                    let k = fact
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim();
                    let v = fact
                        .get("value")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim();
                    if !k.is_empty() || !v.is_empty() {
                        facts.push((k.to_string(), v.to_string()));
                    }
                }
            }
        }
        "Container" => {
            if let Some(items) = obj.get("items").and_then(Value::as_array) {
                for item in items {
                    collect_adaptive_element(item, title, subtitle, facts, body_lines, actions);
                }
            }
        }
        "ColumnSet" => {
            if let Some(cols) = obj.get("columns").and_then(Value::as_array) {
                for col in cols {
                    if let Some(items) = col.get("items").and_then(Value::as_array) {
                        for item in items {
                            collect_adaptive_element(
                                item, title, subtitle, facts, body_lines, actions,
                            );
                        }
                    }
                }
            }
        }
        "ActionSet" => {
            if let Some(arr) = obj.get("actions").and_then(Value::as_array) {
                collect_adaptive_actions(arr, actions);
            }
        }
        "RichTextBlock" => {
            if let Some(inlines) = obj.get("inlines").and_then(Value::as_array) {
                let mut combined = String::new();
                for inline in inlines {
                    if let Some(s) = inline.as_str() {
                        combined.push_str(s);
                    } else if let Some(s) = inline.get("text").and_then(Value::as_str) {
                        combined.push_str(s);
                    }
                }
                let trimmed = combined.trim();
                if !trimmed.is_empty() {
                    body_lines.push(trimmed.to_string());
                }
            }
        }
        "Image" => {
            if let Some(alt) = obj
                .get("altText")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                body_lines.push(format!("[Image: {alt}]"));
            }
        }
        _ => {
            if let Some(items) = obj.get("items").and_then(Value::as_array) {
                for item in items {
                    collect_adaptive_element(item, title, subtitle, facts, body_lines, actions);
                }
            }
        }
    }
}

fn collect_adaptive_actions(arr: &[Value], actions: &mut Vec<(String, String)>) {
    for action in arr {
        let Some(obj) = action.as_object() else {
            continue;
        };
        let action_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
        let label = obj
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Action");

        match action_type {
            "Action.OpenUrl" => {
                let url = obj
                    .get("url")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .unwrap_or("");
                actions.push((label.to_string(), url.to_string()));
            }
            "Action.Submit" => {
                let target = match obj.get("data") {
                    Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
                    Some(v) if !v.is_null() => v.to_string(),
                    _ => "submit".to_string(),
                };
                actions.push((label.to_string(), target));
            }
            "Action.ShowCard" => {
                actions.push((label.to_string(), "show_card".to_string()));
            }
            _ => {
                if let Some(url) = obj.get("url").and_then(Value::as_str) {
                    actions.push((label.to_string(), url.trim().to_string()));
                } else {
                    actions.push((label.to_string(), action_type.to_string()));
                }
            }
        }
    }
}

fn parse_o365_connector(
    obj: &serde_json::Map<String, Value>,
    fallback_name: Option<&str>,
) -> RichBlock {
    let mut title = obj
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let mut subtitle = obj
        .get("summary")
        .or_else(|| obj.get("subtitle"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let mut facts = Vec::new();
    let mut body_lines = Vec::new();
    let mut actions = Vec::new();

    if let Some(text) = obj
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        for line in text.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                body_lines.push(trimmed.to_string());
            }
        }
    }

    if let Some(sections) = obj.get("sections").and_then(Value::as_array) {
        for sec in sections {
            let Some(sec_obj) = sec.as_object() else {
                continue;
            };
            if let Some(sec_title) = sec_obj
                .get("activityTitle")
                .or_else(|| sec_obj.get("title"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                if title.is_none() {
                    title = Some(sec_title.to_string());
                } else {
                    body_lines.push(sec_title.to_string());
                }
            }
            if let Some(sec_sub) = sec_obj
                .get("activitySubtitle")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                if subtitle.is_none() {
                    subtitle = Some(sec_sub.to_string());
                } else {
                    body_lines.push(sec_sub.to_string());
                }
            }
            if let Some(sec_text) = sec_obj
                .get("text")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                for line in sec_text.lines() {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        body_lines.push(trimmed.to_string());
                    }
                }
            }
            if let Some(sec_facts) = sec_obj.get("facts").and_then(Value::as_array) {
                for f in sec_facts {
                    let k = f
                        .get("name")
                        .or_else(|| f.get("title"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim();
                    let v = f
                        .get("value")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim();
                    if !k.is_empty() || !v.is_empty() {
                        facts.push((k.to_string(), v.to_string()));
                    }
                }
            }
            if let Some(sec_actions) = sec_obj.get("potentialAction").and_then(Value::as_array) {
                collect_connector_actions(sec_actions, &mut actions);
            }
        }
    }

    if let Some(top_actions) = obj.get("potentialAction").and_then(Value::as_array) {
        collect_connector_actions(top_actions, &mut actions);
    }

    if title.is_none() {
        title = fallback_name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    }

    RichBlock::Card {
        title,
        subtitle,
        facts,
        body_lines,
        actions,
    }
}

fn collect_connector_actions(arr: &[Value], actions: &mut Vec<(String, String)>) {
    for action in arr {
        let Some(obj) = action.as_object() else {
            continue;
        };
        let name = obj
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Action");
        let uri = obj
            .get("targets")
            .and_then(Value::as_array)
            .and_then(|targets| targets.first())
            .and_then(|t| t.get("uri"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        actions.push((name.to_string(), uri.to_string()));
    }
}
