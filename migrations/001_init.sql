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
