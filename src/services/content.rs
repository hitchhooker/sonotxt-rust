// src/services/content.rs
use crate::{error::{ApiError, Result}, AppState};
use scraper::Html;

pub async fn extract_content(
    state: &AppState,
    url: &str,
    selector: Option<&str>,
) -> Result<String> {
    let parsed = url::Url::parse(url)
        .map_err(|_| ApiError::InvalidUrl)?;
    
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ApiError::InvalidUrl);
    }

    let response = state.http
        .get(url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|_| ApiError::InternalError)?;

    let html = response
        .text()
        .await
        .map_err(|_| ApiError::InternalError)?;

    if html.len() > state.config.max_content_size * 2 {
        return Err(ApiError::ContentTooLarge);
    }

    let document = Html::parse_document(&html);
    
    let text = if let Some(sel) = selector {
        let selector = scraper::Selector::parse(sel)
            .map_err(|_| ApiError::InvalidUrl)?;
        document
            .select(&selector)
            .map(|el| el.text().collect::<String>())
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        ["article", "main", "[role='main']", "body"]
            .iter()
            .find_map(|&s| {
                scraper::Selector::parse(s).ok().and_then(|sel| {
                    document.select(&sel).next().map(|el| {
                        el.text().collect::<String>()
                    })
                })
            })
            .unwrap_or_default()
    };

    let cleaned: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| c.is_ascii() || c.is_alphabetic())
        .take(state.config.max_content_size)
        .collect();

    if cleaned.len() < 100 {
        return Err(ApiError::InvalidUrl);
    }

    Ok(cleaned)
}
