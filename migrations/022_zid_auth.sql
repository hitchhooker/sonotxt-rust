-- ZID identity auth: ed25519 pubkey as primary identity
-- replaces email/passkey for zafu wallet users

ALTER TABLE accounts ADD COLUMN IF NOT EXISTS zid_pubkey VARCHAR(64) UNIQUE;
CREATE INDEX IF NOT EXISTS idx_accounts_zid ON accounts (zid_pubkey) WHERE zid_pubkey IS NOT NULL;
