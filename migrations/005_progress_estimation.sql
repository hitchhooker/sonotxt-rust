-- Add fields for better progress estimation

ALTER TABLE jobs ADD COLUMN IF NOT EXISTS char_count INTEGER;
ALTER TABLE jobs ADD COLUMN IF NOT EXISTS estimated_duration_ms INTEGER;
ALTER TABLE jobs ADD COLUMN IF NOT EXISTS actual_runtime_ms INTEGER;
ALTER TABLE jobs ADD COLUMN IF NOT EXISTS deepinfra_cost DOUBLE PRECISION;
ALTER TABLE jobs ADD COLUMN IF NOT EXISTS deepinfra_request_id TEXT;
