use crate::{error::Result, AppState};
use scraper::{Html, Selector};
use std::sync::Arc;

pub async fn crawl_site(
    state: &Arc<AppState>,
    url: &str,
    selector: Option<&str>,
) -> Result<String> {
    let response = reqwest::get(url).await.map_err(|_| crate::error::ApiError::Internal)?;
    let html = response.text().await.map_err(|_| crate::error::ApiError::Internal)?;
    let document = Html::parse_document(&html);
    
    // Use main content selector or fallback to article/main/body
    let selectors = if let Some(sel) = selector {
        vec![sel]
    } else {
        vec!["main", "article", "[role='main']", ".content", "#content", "body"]
    };
    
    let mut content = String::new();
    
    for selector_str in selectors {
        if let Ok(selector) = Selector::parse(selector_str) {
            for element in document.select(&selector) {
                let extracted = extract_text_recursive(&element);
                if !extracted.is_empty() {
                    content = extracted;
                    break;
                }
            }
            if !content.is_empty() {
                break;
            }
        }
    }
    
    // Clean up - single newlines for paragraphs, double for sections
    let cleaned = content
        .split('\n')
        .map(|line| line.trim())
        .collect::<Vec<_>>()
        .join("\n")
        .split("\n\n\n")  // Replace triple+ newlines with double
        .collect::<Vec<_>>()
        .join("\n\n")
        .split("\n\n")
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    
    if cleaned.is_empty() {
        Err(crate::error::ApiError::NotFound)
    } else {
        Ok(cleaned)
    }
}

fn extract_text_recursive(element: &scraper::ElementRef) -> String {
    let mut result = Vec::new();
    extract_node(&element.clone(), &mut result, 0);
    
    result.join("")
}

fn extract_node(node: &scraper::ElementRef, result: &mut Vec<String>, depth: usize) {
    for child in node.children() {
        match child.value() {
            scraper::Node::Text(text) => {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    result.push(trimmed.to_string());
                    result.push(" ".to_string());
                }
            }
            scraper::Node::Element(elem) => {
                let tag_name = elem.name.local.as_ref();
                
                // Skip script and style tags
                if matches!(tag_name, "script" | "style" | "noscript") {
                    continue;
                }
                
                // Add line breaks before block elements
                if !result.is_empty() {
                    match tag_name {
                        // Major section breaks - double newline
                        "h1" | "h2" | "h3" | "section" | "article" => {
                            result.push("\n\n".to_string());
                        }
                        // Paragraph breaks - single newline  
                        "p" | "div" | "li" | "h4" | "h5" | "h6" => {
                            result.push("\n".to_string());
                        }
                        "br" => {
                            result.push("\n".to_string());
                        }
                        _ => {}
                    }
                }
                
                // Recursively process children
                if let Some(child_elem) = scraper::ElementRef::wrap(child) {
                    extract_node(&child_elem, result, depth + 1);
                }
            }
            _ => {}
        }
    }
}
