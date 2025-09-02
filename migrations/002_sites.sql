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

CREATE INDEX idx_sites_account ON sites(account_id);
CREATE INDEX idx_sites_crawl ON sites(auto_crawl, last_crawled_at);
CREATE INDEX idx_content_site ON content(site_id);
CREATE INDEX idx_content_hash ON content(text_hash);
CREATE INDEX idx_jobs_status ON jobs(status);
