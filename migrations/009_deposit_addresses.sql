-- add from/to address columns for crypto deposits
ALTER TABLE deposits ADD COLUMN IF NOT EXISTS from_address TEXT;
ALTER TABLE deposits ADD COLUMN IF NOT EXISTS to_address TEXT;
