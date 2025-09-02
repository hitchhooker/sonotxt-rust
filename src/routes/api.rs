use axum::{
   extract::{Query, State},
   routing::{get, post},
   Json, Router,
};
use blake3::Hasher;
use std::{collections::HashMap, sync::Arc};

use crate::{
   extractors::AuthenticatedUser,
   models::{JobStatus, ProcessRequest, ProcessResponse},
   services::content::extract_content,
   AppState, Result,
};

pub fn routes() -> Router<Arc<AppState>> {
   Router::new()
       .route("/process", post(process))
       .route("/status", get(status))
}

async fn process(
   State(state): State<Arc<AppState>>,
   AuthenticatedUser(api_key): AuthenticatedUser,
   Json(req): Json<ProcessRequest>,
) -> Result<Json<ProcessResponse>> {
   let content = extract_content(&state, &req.url, req.selector.as_deref()).await?;
   let estimated_cost = (content.len() as f64) * state.config.cost_per_char;

   if api_key.balance < estimated_cost {
       return Err(crate::error::ApiError::InsufficientBalance);
   }

   let mut hasher = Hasher::new();
   hasher.update(content.as_bytes());
   hasher.update(req.url.as_bytes());
   let job_id = hasher.finalize().to_hex()[..16].to_string();

   // Queue job to Redis
   let job_data = serde_json::json!({
       "id": job_id,
       "content": content,
       "api_key": api_key.key,
       "cost": estimated_cost,
   });

   let mut redis = state.redis.clone();
   redis::cmd("LPUSH")
       .arg("tts_queue")
       .arg(job_data.to_string())
       .query_async::<_, ()>(&mut redis)
       .await
       .map_err(|_| crate::error::ApiError::Internal)?;

   Ok(Json(ProcessResponse {
       job_id,
       status: JobStatus::Queued,
       estimated_cost,
   }))
}

async fn status(
   State(state): State<Arc<AppState>>,
   Query(params): Query<HashMap<String, String>>,
) -> Result<Json<JobStatus>> {
   let job_id = params
       .get("job_id")
       .ok_or(crate::error::ApiError::NotFound)?;

   let mut redis = state.redis.clone();
   let status_key = format!("job:{}", job_id);
   
   let status: Option<String> = redis::cmd("GET")
       .arg(&status_key)
       .query_async(&mut redis)
       .await
       .map_err(|_| crate::error::ApiError::Internal)?;

   match status {
       Some(s) => {
           let status: JobStatus = serde_json::from_str(&s)
               .map_err(|_| crate::error::ApiError::Internal)?;
           Ok(Json(status))
       }
       None => Ok(Json(JobStatus::Queued))
   }
}
