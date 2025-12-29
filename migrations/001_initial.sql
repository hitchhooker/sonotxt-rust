-- Initial schema for SonoTxt

-- Accounts table (users)
CREATE TABLE IF NOT EXISTS accounts (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    email TEXT UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- API keys for authentication
CREATE TABLE IF NOT EXISTS api_keys (
    key TEXT PRIMARY KEY,
    account_id UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    revoked BOOLEAN NOT NULL DEFAULT FALSE,
    last_used_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_api_keys_account ON api_keys(account_id);

-- Account credits and billing
CREATE TABLE IF NOT EXISTS account_credits (
    account_id UUID PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    balance DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    subscription_type TEXT,
    subscription_expires TIMESTAMPTZ,
    watermark_free BOOLEAN NOT NULL DEFAULT FALSE,
    stripe_customer_id TEXT,
    stripe_subscription_id TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Sites registered for crawling
CREATE TABLE IF NOT EXISTS sites (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    url TEXT NOT NULL,
    selector TEXT,
    auto_crawl BOOLEAN NOT NULL DEFAULT FALSE,
    crawl_frequency_hours INTEGER NOT NULL DEFAULT 24,
    last_crawled_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_sites_account ON sites(account_id);

-- Content extracted from sites
CREATE TABLE IF NOT EXISTS content (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    site_id UUID REFERENCES sites(id) ON DELETE CASCADE,
    url TEXT NOT NULL,
    text_content TEXT NOT NULL,
    text_hash TEXT NOT NULL,
    word_count INTEGER NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_content_site ON content(site_id);
CREATE INDEX IF NOT EXISTS idx_content_hash ON content(text_hash);

-- Content version history
CREATE TABLE IF NOT EXISTS content_versions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    content_id UUID NOT NULL REFERENCES content(id) ON DELETE CASCADE,
    text_content TEXT NOT NULL,
    text_hash TEXT NOT NULL,
    word_count INTEGER NOT NULL,
    version_type TEXT NOT NULL, -- 'crawl', 'edit', 'auto_clean'
    created_by UUID REFERENCES accounts(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_content_versions_content ON content_versions(content_id);

-- TTS processing jobs
CREATE TABLE IF NOT EXISTS jobs (
    id TEXT PRIMARY KEY,
    content_id UUID REFERENCES content(id) ON DELETE SET NULL,
    api_key TEXT NOT NULL,
    text_content TEXT,
    voice TEXT NOT NULL DEFAULT 'af_bella',
    status TEXT NOT NULL DEFAULT 'queued',
    audio_url TEXT,
    duration_seconds DOUBLE PRECISION,
    cost DOUBLE PRECISION,
    error_message TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_jobs_status ON jobs(status);
CREATE INDEX IF NOT EXISTS idx_jobs_api_key ON jobs(api_key);
CREATE INDEX IF NOT EXISTS idx_jobs_created ON jobs(created_at);

-- Transaction log for billing
CREATE TABLE IF NOT EXISTS transactions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    amount DOUBLE PRECISION NOT NULL,
    type TEXT NOT NULL, -- 'purchase', 'usage', 'subscription', 'refund'
    description TEXT,
    stripe_payment_id TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_transactions_account ON transactions(account_id);
CREATE INDEX IF NOT EXISTS idx_transactions_created ON transactions(created_at);
