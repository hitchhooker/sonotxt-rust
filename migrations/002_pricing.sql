-- migrations/002_pricing.sql
CREATE TABLE pricing_plans (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name VARCHAR(50) NOT NULL,
    tier VARCHAR(20) NOT NULL, -- 'free', 'starter', 'pro', 'business'
    price_monthly DECIMAL(10,2),
    minutes_daily INTEGER,
    minutes_monthly INTEGER,
    rollover BOOLEAN DEFAULT false,
    watermark BOOLEAN DEFAULT true,
    custom_voice BOOLEAN DEFAULT false,
    priority_queue BOOLEAN DEFAULT false,
    api_access BOOLEAN DEFAULT false,
    embed_analytics BOOLEAN DEFAULT false,
    max_article_length INTEGER, -- characters
    created_at TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE account_plans (
    account_id UUID PRIMARY KEY,
    plan_id UUID REFERENCES pricing_plans(id),
    minutes_used_today INTEGER DEFAULT 0,
    minutes_used_month INTEGER DEFAULT 0,
    rollover_minutes INTEGER DEFAULT 0,
    daily_reset_at TIMESTAMPTZ,
    monthly_reset_at TIMESTAMPTZ,
    stripe_subscription_id VARCHAR(255),
    created_at TIMESTAMPTZ DEFAULT NOW(),
    updated_at TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE usage_logs (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id UUID NOT NULL,
    content_id UUID,
    minutes_consumed DECIMAL(10,2),
    characters_processed INTEGER,
    model_used VARCHAR(20), -- '1.5b', '7b'
    created_at TIMESTAMPTZ DEFAULT NOW()
);

-- Seed pricing plans
INSERT INTO pricing_plans (name, tier, price_monthly, minutes_daily, minutes_monthly, rollover, watermark, custom_voice, priority_queue, api_access, embed_analytics, max_article_length) VALUES
('Free', 'free', 0, 5, 150, false, true, false, false, false, false, 5000),
('Starter', 'starter', 9, 10, 300, true, true, false, false, false, false, 15000),
('Pro', 'pro', 29, 30, 900, true, false, true, true, false, true, 50000),
('Business', 'business', 99, 100, 3000, true, false, true, true, true, true, 100000);
