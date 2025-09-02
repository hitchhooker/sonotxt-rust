CREATE EXTENSION IF NOT EXISTS "uuid-ossp";

CREATE TABLE accounts (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    email TEXT UNIQUE NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE api_keys (
    key TEXT PRIMARY KEY,
    account_id UUID NOT NULL REFERENCES accounts(id),
    balance DECIMAL(10,4) NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    active BOOLEAN NOT NULL DEFAULT true
);

CREATE TABLE sites (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    account_id UUID NOT NULL REFERENCES accounts(id),
    url TEXT NOT NULL,
    selector TEXT,
    auto_crawl BOOLEAN DEFAULT false,
    crawl_frequency_hours INT DEFAULT 24,
    last_crawled_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(account_id, url)
);

CREATE TABLE content (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    site_id UUID REFERENCES sites(id) ON DELETE CASCADE,
    url TEXT NOT NULL,
    title TEXT,
    text_content TEXT NOT NULL,
    text_hash TEXT NOT NULL,
    word_count INT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(text_hash)
);

CREATE TABLE content_versions (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    content_id UUID REFERENCES content(id) ON DELETE CASCADE,
    text_content TEXT NOT NULL,
    text_hash TEXT NOT NULL,
    word_count INT NOT NULL,
    version_type TEXT NOT NULL CHECK (version_type IN ('crawl', 'edit', 'auto_clean')),
    created_by UUID REFERENCES accounts(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE jobs (
    id TEXT PRIMARY KEY,
    content_id UUID REFERENCES content(id),
    api_key TEXT REFERENCES api_keys(key),
    status TEXT NOT NULL DEFAULT 'queued',
    audio_url TEXT,
    duration_seconds FLOAT,
    cost DECIMAL(10,4),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ
);

-- Indexes
CREATE INDEX idx_sites_account ON sites(account_id);
CREATE INDEX idx_sites_crawl ON sites(auto_crawl, last_crawled_at);
CREATE INDEX idx_content_site ON content(site_id);
CREATE INDEX idx_content_hash ON content(text_hash);
CREATE INDEX idx_content_versions ON content_versions(content_id, created_at DESC);
CREATE INDEX idx_version_type ON content_versions(version_type);
CREATE INDEX idx_jobs_status ON jobs(status);

-- Dev account
INSERT INTO accounts (email) VALUES ('dev@sonotxt.com');
INSERT INTO api_keys (key, account_id) 
SELECT 'dev-token-123', id FROM accounts WHERE email = 'dev@sonotxt.com';
