-- add penumbra_index column for mapping penumbra deposits back to accounts
-- penumbra uses address indices internally which we store for deposit matching
ALTER TABLE payment_addresses ADD COLUMN IF NOT EXISTS penumbra_index BIGINT;

-- create index for efficient lookup during deposit processing
CREATE INDEX IF NOT EXISTS idx_payment_addresses_penumbra_index
ON payment_addresses(penumbra_index) WHERE penumbra_index IS NOT NULL;
