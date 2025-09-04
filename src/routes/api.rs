use axum::{
   extract::{Query, State},
   routing::{get, post},
   Json, Router,
};
use std::{collections::HashMap, sync::Arc};
use uuid::Uuid;

use crate::{error::Result,
   extractors::AuthenticatedUser,
   models::{JobStatus, ProcessRequest, ProcessResponse},
   services::content::extract_content,
   AppState,
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

   let job_id = Uuid::new_v4().to_string();
   
   sqlx::query!(
       "INSERT INTO jobs (id, api_key, text_content, status) VALUES ($1, $2, $3, 'queued')",
       job_id,
       api_key.key,
       content.as_str()
   )
   .execute(&state.db)
   .await?;
   
   // Insert directly into jobs table
   sqlx::query!(
   )
   .execute(&state.db)
   .await?;

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

   let job = sqlx::query!(
       "SELECT status FROM jobs WHERE id = $1",
       job_id
   )
   .fetch_optional(&state.db)
   .await?;
   
   match job {
       Some(j) => match j.status.as_deref() {
           Some("completed") => Ok(Json(JobStatus::Complete { 
               url: format!("https://storage.sonotxt.com/audio/{}.mp3", job_id),
               duration_seconds: 0.0 
           })),
           Some("failed") => Ok(Json(JobStatus::Failed { reason: "Processing failed".into() })),
           _ => Ok(Json(JobStatus::Queued)),
       },
       None => Ok(Json(JobStatus::Queued))
   }
}
