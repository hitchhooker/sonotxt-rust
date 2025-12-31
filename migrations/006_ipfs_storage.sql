-- Add IPFS storage support
-- storage_type: 'minio' (default) or 'ipfs'
-- ipfs_cid: Content ID for IPFS stored audio
-- crust_order_id: Order ID from Crust pinning
-- pinning_cost: Cost deducted from user balance for pinning

ALTER TABLE jobs ADD COLUMN IF NOT EXISTS storage_type TEXT DEFAULT 'minio';
ALTER TABLE jobs ADD COLUMN IF NOT EXISTS ipfs_cid TEXT;
ALTER TABLE jobs ADD COLUMN IF NOT EXISTS crust_order_id TEXT;
ALTER TABLE jobs ADD COLUMN IF NOT EXISTS pinning_cost DOUBLE PRECISION;

-- Index for IPFS lookups
CREATE INDEX IF NOT EXISTS idx_jobs_ipfs_cid ON jobs(ipfs_cid) WHERE ipfs_cid IS NOT NULL;
