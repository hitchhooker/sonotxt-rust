ALTER TABLE jobs ADD COLUMN IF NOT EXISTS priority INTEGER NOT NULL DEFAULT 0;
CREATE INDEX IF NOT EXISTS idx_jobs_queue_priority
  ON jobs(priority DESC, created_at ASC) WHERE status = 'queued';
