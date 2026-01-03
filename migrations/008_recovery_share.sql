-- add recovery share for shamir secret sharing based account recovery
-- the server stores one half of the XOR split of the user's seed
-- the user stores the other half as recovery words
-- both halves are needed to reconstruct the seed

ALTER TABLE users ADD COLUMN IF NOT EXISTS recovery_share TEXT;

-- index for looking up users by email for recovery
CREATE INDEX IF NOT EXISTS idx_users_email ON users(email);
