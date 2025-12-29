-- Free tier usage tracking by IP hash
-- Allows 1000 chars per day per IP (hashed for privacy)

CREATE TABLE IF NOT EXISTS free_tier_usage (
    ip_hash TEXT PRIMARY KEY,
    chars_used INTEGER NOT NULL DEFAULT 0,
    last_reset DATE NOT NULL DEFAULT CURRENT_DATE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_free_tier_last_reset ON free_tier_usage(last_reset);

-- Track free tier jobs separately (no api_key required)
ALTER TABLE jobs ALTER COLUMN api_key DROP NOT NULL;
ALTER TABLE jobs ADD COLUMN IF NOT EXISTS ip_hash TEXT;
ALTER TABLE jobs ADD COLUMN IF NOT EXISTS is_free_tier BOOLEAN NOT NULL DEFAULT FALSE;

CREATE INDEX IF NOT EXISTS idx_jobs_ip_hash ON jobs(ip_hash) WHERE is_free_tier = TRUE;
