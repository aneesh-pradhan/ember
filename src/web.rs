//! Web access for the agent: search through Tavily, and fetching pages as readable text.

use std::time::Duration;

use regex::Regex;
use serde_json::{json, Value};

const TAVILY: &str = "https://api.tavily.com";
const MAX_PAGE: usize = 30_000;

fn tavily_key() -> Option<String> {
    std::env::var("TAVILY_API_KEY").ok().filter(|k| !k.trim().is_empty())
}

/// EMBER_NO_WEB=1 removes both web tools, e.g. for benchmarks where the upstream fix is online.
pub fn enabled() -> bool {
    !matches!(std::env::var("EMBER_NO_WEB").as_deref(), Ok("1" | "true" | "on"))
}

/// web_search needs a Tavily key; web_fetch works whenever web tools are enabled.
pub fn search_available() -> bool {
    enabled() && tavily_key().is_some()
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(45))
        .user_agent(concat!("ember/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("http client")
}

async fn tavily(endpoint: &str, body: Value) -> Result<Value, String> {
    let key = tavily_key().ok_or("TAVILY_API_KEY is not set")?;
    let resp = client()
        .post(format!("{TAVILY}/{endpoint}"))
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Tavily request failed: {e}"))?;
    let status = resp.status();
    let value: Value = resp.json().await.map_err(|e| format!("Tavily returned invalid JSON: {e}"))?;
    if !status.is_success() {
        return Err(format!("Tavily error {status}: {}", value["detail"].as_str().unwrap_or(&value.to_string())));
    }
    Ok(value)
}

pub async fn search(args: &Value) -> Result<String, String> {
    let query = args["query"].as_str().filter(|q| !q.trim().is_empty()).ok_or("query is required")?;
    let topic = args["topic"].as_str().filter(|t| matches!(*t, "news" | "finance")).unwrap_or("general");
    let max = args["max_results"].as_u64().unwrap_or(5).clamp(1, 10);
    let v = tavily("search", json!({
        "query": query, "topic": topic, "max_results": max,
        "search_depth": "basic", "include_answer": "basic",
    }))
    .await?;
    let mut out = String::new();
    if let Some(answer) = v["answer"].as_str().filter(|a| !a.is_empty()) {
        out.push_str(&format!("Summary: {answer}\n\n"));
    }
    for (i, r) in v["results"].as_array().into_iter().flatten().enumerate() {
        out.push_str(&format!(
            "{}. {}\n   {}\n   {}\n\n",
            i + 1,
            r["title"].as_str().unwrap_or("(untitled)"),
            r["url"].as_str().unwrap_or(""),
            r["content"].as_str().unwrap_or("").trim()
        ));
    }
    if out.is_empty() {
        out = "No results.".into();
    }
    Ok(out)
}

pub async fn fetch(args: &Value) -> Result<String, String> {
    let url = args["url"].as_str().map(str::trim).ok_or("url is required")?;
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("url must start with http:// or https://".into());
    }
    let text = if search_available() {
        let v = tavily("extract", json!({"urls": [url], "format": "markdown"})).await?;
        match v["results"].as_array().and_then(|r| r.first()) {
            Some(r) => r["raw_content"].as_str().unwrap_or("").to_string(),
            None => {
                let err = v["failed_results"][0]["error"].as_str().unwrap_or("unknown error");
                return Err(format!("could not extract {url}: {err}"));
            }
        }
    } else {
        let resp = client().get(url).send().await.map_err(|e| format!("request failed: {e}"))?;
        let status = resp.status();
        let is_html = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.contains("html"));
        let body = resp.text().await.map_err(|e| format!("could not read body: {e}"))?;
        if !status.is_success() {
            return Err(format!("HTTP {status} for {url}"));
        }
        if is_html { html_to_text(&body) } else { body }
    };
    Ok(crate::tools::truncate(text.trim(), MAX_PAGE))
}

/// A rough HTML-to-text conversion: drop scripts and styles, keep block structure as line breaks.
pub fn html_to_text(html: &str) -> String {

    let blocks = Regex::new(r"(?i)<\s*(br|/p|/div|/h[1-6]|/tr|/section|/article|/pre)\s*/?>").unwrap();
    let items = Regex::new(r"(?i)<\s*li\b[^>]*>").unwrap();
    let tags = Regex::new(r"(?s)<[^>]+>").unwrap();
    let spaces = Regex::new(r"[ \t\u{a0}]+").unwrap();
    let blank = Regex::new(r"\n\s*\n\s*\n+").unwrap();

    let mut text = html.to_string();
    for tag in ["script", "style", "noscript", "svg", "head"] {
        let re = Regex::new(&format!(r"(?is)<{tag}\b.*?</{tag}\s*>")).unwrap();
        text = re.replace_all(&text, "").into_owned();
    }
    let text = blocks.replace_all(&text, "\n");
    let text = items.replace_all(&text, "\n• ");
    let text = tags.replace_all(&text, "");
    let text = text
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    let text = spaces.replace_all(&text, " ");
    let text: String = text.lines().map(str::trim).collect::<Vec<_>>().join("\n");
    blank.replace_all(&text, "\n\n").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_html() {
        let html = "<html><head><title>x</title><style>p{}</style></head><body><h1>Title</h1>\
                    <p>Hello &amp; <b>welcome</b></p><ul><li>one</li><li>two</li></ul><script>evil()</script></body></html>";
        assert_eq!(html_to_text(html), "Title\nHello & welcome\n\n• one\n• two");
    }
}
