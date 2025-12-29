-- Auth sessions for magic link login
CREATE TABLE IF NOT EXISTS auth_sessions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    token TEXT NOT NULL UNIQUE,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_auth_sessions_token ON auth_sessions(token);
CREATE INDEX IF NOT EXISTS idx_auth_sessions_account ON auth_sessions(account_id);

-- Magic link tokens (short-lived, single use)
CREATE TABLE IF NOT EXISTS magic_links (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    email TEXT NOT NULL,
    token TEXT NOT NULL UNIQUE,
    used BOOLEAN NOT NULL DEFAULT FALSE,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_magic_links_token ON magic_links(token);
CREATE INDEX IF NOT EXISTS idx_magic_links_email ON magic_links(email);

-- Payment addresses per account (for crypto deposits)
CREATE TABLE IF NOT EXISTS payment_addresses (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    chain TEXT NOT NULL, -- 'polkadot_assethub', 'penumbra'
    address TEXT NOT NULL,
    derivation_index INTEGER NOT NULL DEFAULT 0, -- for wallet rotation
    is_active BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(account_id, chain, derivation_index)
);

CREATE INDEX IF NOT EXISTS idx_payment_addresses_account ON payment_addresses(account_id);
CREATE INDEX IF NOT EXISTS idx_payment_addresses_address ON payment_addresses(address);

-- Crypto deposits (tracked incoming payments)
CREATE TABLE IF NOT EXISTS deposits (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    chain TEXT NOT NULL, -- 'polkadot_assethub', 'penumbra', 'stripe'
    tx_hash TEXT NOT NULL UNIQUE,
    asset TEXT NOT NULL, -- 'USDC', 'USDT', 'USD'
    amount DOUBLE PRECISION NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending', -- 'pending', 'confirmed', 'credited', 'failed'
    block_number BIGINT,
    confirmations INTEGER NOT NULL DEFAULT 0,
    credited_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_deposits_account ON deposits(account_id);
CREATE INDEX IF NOT EXISTS idx_deposits_status ON deposits(status);
CREATE INDEX IF NOT EXISTS idx_deposits_tx_hash ON deposits(tx_hash);

-- Extend transactions table to support crypto payments
ALTER TABLE transactions ADD COLUMN IF NOT EXISTS chain TEXT;
ALTER TABLE transactions ADD COLUMN IF NOT EXISTS tx_hash TEXT;
ALTER TABLE transactions ADD COLUMN IF NOT EXISTS deposit_id UUID REFERENCES deposits(id);

-- Rename stripe_payment_id to payment_id (more generic)
-- Note: keeping stripe_payment_id for backwards compat, adding payment_id
ALTER TABLE transactions ADD COLUMN IF NOT EXISTS payment_id TEXT;
