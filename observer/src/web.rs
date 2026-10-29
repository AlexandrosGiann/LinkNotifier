//! Related reading from Wikipedia, used to widen the quiz.
//!
//! Wikipedia has a keyless public API, articles in every language Whistle
//! knows, and plain-text extracts. Only key phrases of a topic leave the
//! computer, never the transcript itself.

use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub title: String,
    pub url: String,
    /// The topic it was found for.
    pub topic: usize,
    /// MiniLM similarity of the article to the topic.
    pub score: f32,
    /// Sentences of the article closest to the topic.
    pub sentences: Vec<String>,
    /// What the first sentence says the subject is: "a type of feedforward
    /// neural network that learns features via filter optimization".
    #[serde(default)]
    pub definition: String,
    /// "CNN" when the first sentence gives one.
    #[serde(default)]
    pub acronym: String,
    /// The first sentence starts with "A"/"An": "What is a CNN?", not "What is backpropagation?".
    #[serde(default)]
    pub countable: bool,
    /// Related but not about the topic: used only for wrong options.
    #[serde(default)]
    pub neighbor: bool,
}

/// The first sentence of an article.
pub fn first_sentence(text: &str) -> String {
    let chars: Vec<char> = text.trim().chars().collect();
    let mut out = String::new();
    let mut depth = 0i32;
    for (i, &c) in chars.iter().enumerate() {
        out.push(c);
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            '.' if depth <= 0 => {
                let next = chars.get(i + 1).copied().unwrap_or(' ');
                if next.is_whitespace() {
                    break;
                }
            }
            '\n' => break,
            _ => {}
        }
    }
    out.trim().to_string()
}

/// Splits "X (ACR) is a ..." into the definition, the acronym and whether X takes an article.
pub fn definition(first: &str) -> Option<(String, String, bool)> {
    let lower = first.to_lowercase();
    let cut = [" is a ", " is an ", " is the ", " are ", " is ", " refers to "]
        .iter()
        .filter_map(|m| lower.find(m).map(|i| (i, m.len())))
        .min_by_key(|x| x.0)?;
    let subject = &first[..cut.0];
    let mut def = first[cut.0 + cut.1..].trim().trim_end_matches('.').to_string();
    let verb = &lower[cut.0..cut.0 + cut.1];
    if verb.starts_with(" is a") || verb.starts_with(" is the") {
        // Keep the article: "a type of ...".
        def = first[cut.0 + 4..].trim().trim_end_matches('.').to_string();
    }
    if def.split_whitespace().count() < 4 {
        return None;
    }
    let acronym = subject
        .split('(')
        .nth(1)
        .and_then(|r| r.split(')').next())
        .map(|a| a.trim().trim_end_matches('s').to_string())
        .filter(|a| (2..=6).contains(&a.len()) && a.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()))
        .unwrap_or_default();
    let countable = lower.starts_with("a ") || lower.starts_with("an ") || lower.contains(", a ") && lower.find(", a ").is_some_and(|i| i < cut.0);
    Some((def, acronym, countable))
}

const USER_AGENT: &str = "LinkNotifier-Observer/0.1 (local study notes; https://github.com/AlexandrosGiann)";
const TIMEOUT: Duration = Duration::from_secs(10);

fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn get_json(url: &str) -> Result<serde_json::Value, String> {
    ureq::get(url)
        .set("User-Agent", USER_AGENT)
        .timeout(TIMEOUT)
        .call()
        .map_err(|e| e.to_string())?
        .into_json()
        .map_err(|e| e.to_string())
}

/// Titles of the best matches for `query`.
pub fn search(lang: &str, query: &str, limit: usize) -> Result<Vec<String>, String> {
    let url = format!(
        "https://{lang}.wikipedia.org/w/api.php?action=query&list=search&format=json&utf8=1&srlimit={limit}&srsearch={}",
        encode(query)
    );
    let v = get_json(&url)?;
    Ok(v["query"]["search"]
        .as_array()
        .map(|a| a.iter().filter_map(|r| r["title"].as_str().map(str::to_string)).collect())
        .unwrap_or_default())
}

/// Plain text of an article (capped) and its URL.
pub fn article(lang: &str, title: &str) -> Result<(String, String), String> {
    let url = format!(
        "https://{lang}.wikipedia.org/w/api.php?action=query&prop=extracts|info&inprop=url&explaintext=1&exsectionformat=plain&redirects=1&format=json&utf8=1&titles={}",
        encode(title)
    );
    let v = get_json(&url)?;
    let pages = v["query"]["pages"].as_object().ok_or("no pages")?;
    let page = pages.values().next().ok_or("no page")?;
    let text: String = page["extract"].as_str().unwrap_or("").chars().take(30_000).collect();
    let link = page["fullurl"].as_str().unwrap_or("").to_string();
    if text.is_empty() {
        return Err(format!("{title}: empty"));
    }
    Ok((text, link))
}

/// Sentences of a plain-text article, without headings and fragments.
pub fn sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.split('\n') {
        let para = para.trim();
        if para.split_whitespace().count() < 6 {
            continue;
        }
        let mut cur = String::new();
        let chars: Vec<char> = para.chars().collect();
        for (i, &c) in chars.iter().enumerate() {
            cur.push(c);
            let next = chars.get(i + 1).copied().unwrap_or(' ');
            let after = chars.get(i + 2).copied().unwrap_or('A');
            if matches!(c, '.' | '!' | '?') && next == ' ' && after.is_uppercase() {
                out.push(cur.trim().to_string());
                cur.clear();
            }
        }
        if !cur.trim().is_empty() {
            out.push(cur.trim().to_string());
        }
    }
    out.retain(|s| {
        let n = s.split_whitespace().count();
        (6..=40).contains(&n)
    });
    out
}
