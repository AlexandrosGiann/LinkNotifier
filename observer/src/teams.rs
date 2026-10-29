//! Sending the notes to a Teams channel or chat.
//!
//! Uses a Teams *Workflows* webhook ("Post to a channel when a webhook request
//! is received"), which replaced the retired Office 365 connectors. No Azure
//! app registration or admin consent is needed: the user creates the workflow
//! in Teams and pastes its URL into the app settings.

use serde_json::{json, Value};

use crate::notes::{clock, item_line, topic_heading, Notes, ITEM_ORDER};

/// Teams rejects messages over about 28 KB.
const MAX_CARD_BYTES: usize = 24_000;

fn text(t: &str, extra: Value) -> Value {
    let mut v = json!({ "type": "TextBlock", "text": t, "wrap": true });
    if let (Value::Object(m), Value::Object(e)) = (&mut v, extra) {
        m.extend(e);
    }
    v
}

pub fn adaptive_card(n: &Notes) -> Value {
    let mut body = vec![
        text(&format!("📝 Σημειώσεις συνάντησης — {}", n.started), json!({ "size": "Large", "weight": "Bolder" })),
        json!({ "type": "FactSet", "facts": [
            { "title": "Διάρκεια", "value": clock(n.duration) },
            { "title": "Teams", "value": if n.teams_title.is_empty() { "-" } else { &n.teams_title } },
            { "title": "Τμήματα ομιλίας", "value": n.segments.len().to_string() },
        ]}),
    ];
    if !n.overview.is_empty() {
        body.push(text("Σύνοψη", json!({ "weight": "Bolder", "spacing": "Medium" })));
        body.push(text(&n.overview, json!({})));
    }
    for kind in ITEM_ORDER {
        let lines: Vec<String> = n.items.iter().filter(|i| i.kind == kind).map(|i| format!("- {}", item_line(i))).collect();
        if lines.is_empty() {
            continue;
        }
        body.push(text(kind.title(), json!({ "weight": "Bolder", "spacing": "Medium" })));
        body.push(text(&lines.join("\n"), json!({})));
    }
    if !n.topics.is_empty() {
        body.push(text("Θέματα", json!({ "weight": "Bolder", "spacing": "Medium" })));
        for (i, t) in n.topics.iter().enumerate() {
            body.push(text(&format!("**{}**\n{}", topic_heading(i + 1, t), t.summary), json!({})));
        }
    }
    let mut card = json!({
        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
        "type": "AdaptiveCard",
        "version": "1.4",
        "msteams": { "width": "Full" },
        "body": body,
    });
    // Drop topics from the end until the card fits.
    while serde_json::to_string(&card).map(|s| s.len()).unwrap_or(0) > MAX_CARD_BYTES {
        let Some(b) = card["body"].as_array_mut() else { break };
        if b.len() <= 3 {
            break;
        }
        b.pop();
    }
    card
}

/// The message envelope a Workflows webhook expects.
pub fn message(card: &Value) -> Value {
    json!({
        "type": "message",
        "attachments": [{
            "contentType": "application/vnd.microsoft.card.adaptive",
            "contentUrl": null,
            "content": card,
        }]
    })
}

pub fn post(webhook: &str, card: &Value) -> Result<(), String> {
    if !webhook.starts_with("https://") {
        return Err("the webhook URL must start with https://".into());
    }
    match ureq::post(webhook).timeout(std::time::Duration::from_secs(30)).send_json(message(card)) {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(code, r)) => Err(format!("Teams answered HTTP {}: {}", code, r.into_string().unwrap_or_default())),
        Err(e) => Err(e.to_string()),
    }
}
