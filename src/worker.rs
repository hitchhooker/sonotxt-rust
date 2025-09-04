use crate::{AppState, error::Result};
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use uuid::Uuid;

pub async fn run_worker(state: Arc<AppState>) {
    println!("Starting TTS worker...");
    
    loop {
        if let Err(e) = process_next_job(&state).await {
            eprintln!("Worker error: {:?}", e);
        }
        
        sleep(Duration::from_secs(5)).await;
    }
}

async fn process_next_job(state: &Arc<AppState>) -> Result<()> {
    // Get next queued job
    let job = sqlx::query!(
        r#"
        UPDATE jobs 
        SET status = 'processing'
        WHERE id = (
            SELECT id FROM jobs 
            WHERE status = 'queued' 
            ORDER BY created_at 
            LIMIT 1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id, content_id, api_key
        "#
    )
    .fetch_optional(&state.db)
    .await.map_err(|_| crate::error::ApiError::Internal)?;
    
    let Some(job) = job else {
        return Ok(()); // No jobs
    };
    
    println!("Processing job: {}", job.id);
    
    // Get content
    let content = sqlx::query!(
        "SELECT text_content, word_count FROM content WHERE id = $1",
        job.content_id.unwrap()
    )
    .fetch_one(&state.db)
    .await.map_err(|_| crate::error::ApiError::Internal)?;
    
    // Here you'd call actual TTS API
    // For now, simulate with delay
    let duration_seconds = (content.word_count as f64 / 150.0) * 60.0; // ~150 WPM
    let cost = (content.text_content.len() as f64) * state.config.cost_per_char;
    
    sleep(Duration::from_secs(2)).await; // Simulate processing
    
    // Generate fake audio URL
    let audio_url = format!("https://storage.sonotxt.com/audio/{}.mp3", job.id);
    
    // Update job as completed
    sqlx::query!(
        r#"
        UPDATE jobs 
        SET status = 'completed',
            audio_url = $1,
            duration_seconds = $2,
            cost = $3,
            completed_at = NOW()
        WHERE id = $4
        "#,
        audio_url,
        duration_seconds,
        cost,
        job.id
    )
    .execute(&state.db)
    .await.map_err(|_| crate::error::ApiError::Internal)?;
    
    // Deduct balance
    sqlx::query!(
        "UPDATE api_keys SET balance = balance - $1 WHERE key = $2",
        cost,
        job.api_key
    )
    .execute(&state.db)
    .await.map_err(|_| crate::error::ApiError::Internal)?;
    
    println!("Job {} completed. Cost: ${:.4}", job.id, cost);
    
    Ok(())
}

// For actual TTS integration
pub async fn generate_tts(text: &str, voice: &str) -> Result<Vec<u8>> {
    // OpenAI TTS example:
    /*
    let client = reqwest::Client::new();
    let response = client.post("https://api.openai.com/v1/audio/speech")
        .header("Authorization", format!("Bearer {}", api_key))
        .json(&json!({
            "model": "tts-1-hd",
            "input": text,
            "voice": voice,
            "response_format": "mp3"
        }))
        .send()
        .await.map_err(|_| crate::error::ApiError::Internal)?;
    
    Ok(response.bytes().await.map_err(|_| crate::error::ApiError::Internal)?.to_vec())
    */
    
    // Placeholder
    Ok(vec![])
}
