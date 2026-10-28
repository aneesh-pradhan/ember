//! Images: Nemotron VL looks at a screenshot or diagram and describes it for the coding model.

use std::path::Path;

use anyhow::{bail, Context, Result};
use base64::Engine;
use serde_json::json;

use crate::llm::{Client, Usage};

const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;

pub fn is_image(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp")
    )
}

fn mime(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "image/png",
    }
}

/// Ask the vision model to describe `path` with the user's request in mind.
pub async fn describe(client: &Client, model: &str, path: &Path, request: &str) -> Result<(String, Usage)> {
    let size = std::fs::metadata(path).with_context(|| format!("{} not found", path.display()))?.len();
    if size > MAX_IMAGE_BYTES {
        bail!("{} is larger than 8 MB", path.display());
    }
    let data = base64::engine::general_purpose::STANDARD.encode(std::fs::read(path)?);
    let prompt = format!(
        "You are the eyes of a coding agent that cannot see images. Describe this image so the agent can act on \
         the request below. Transcribe all visible text exactly (error messages, code, logs, labels, numbers). \
         Describe layout, components, colors and anything that looks wrong or notable. Be precise and concise.\n\n\
         Request: {request}"
    );
    let body = json!({
        "model": model,
        "max_tokens": 6000,
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": prompt},
            {"type": "image_url", "image_url": {"url": format!("data:{};base64,{data}", mime(path))}},
        ]}],
    });
    let (content, usage) = client.complete_raw(body).await?;
    if content.trim().is_empty() {
        bail!("the vision model returned no description");
    }
    Ok((content, usage))
}
