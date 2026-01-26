-- Encrypted vault storage for paid users
-- Server stores encrypted blobs it cannot decrypt
-- Client encrypts with PRF-derived key before upload

CREATE TABLE vault_items (
    id TEXT PRIMARY KEY,
    account_id UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    filename TEXT NOT NULL,
    size_bytes BIGINT NOT NULL,
    content_type TEXT NOT NULL DEFAULT 'application/octet-stream',
    storage_key TEXT NOT NULL,
    is_public BOOLEAN NOT NULL DEFAULT FALSE,
    public_url TEXT,
    ipfs_cid TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_vault_items_account ON vault_items(account_id);
CREATE INDEX idx_vault_items_public ON vault_items(is_public) WHERE is_public = TRUE;

-- Trigger to update updated_at
CREATE OR REPLACE FUNCTION update_vault_items_updated_at()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER vault_items_updated_at
    BEFORE UPDATE ON vault_items
    FOR EACH ROW
    EXECUTE FUNCTION update_vault_items_updated_at();
