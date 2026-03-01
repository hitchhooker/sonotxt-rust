-- add engine column to jobs table for multi-backend tts support
ALTER TABLE jobs
ADD COLUMN IF NOT EXISTS engine TEXT DEFAULT 'kokoro';

-- create index for engine filtering
CREATE INDEX IF NOT EXISTS idx_jobs_engine ON jobs(engine);

-- add comment
COMMENT ON COLUMN jobs.engine IS 'TTS engine: kokoro, vibevoice, or vibevoice-streaming';
