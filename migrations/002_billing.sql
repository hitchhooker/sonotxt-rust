-- migrations/002_billing.sql
CREATE TABLE account_credits (
    account_id UUID PRIMARY KEY,
    balance DECIMAL(10,2) DEFAULT 5.00,
    subscription_type VARCHAR(20), -- NULL, 'monthly', 'yearly'
    subscription_expires TIMESTAMPTZ,
    watermark_free BOOLEAN DEFAULT false,
    stripe_customer_id VARCHAR(255),
    stripe_subscription_id VARCHAR(255),
    created_at TIMESTAMPTZ DEFAULT NOW(),
    updated_at TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE pricing (
    feature VARCHAR(50) PRIMARY KEY,
    credit_cost DECIMAL(10,4),
    subscriber_cost DECIMAL(10,4)
);

INSERT INTO pricing VALUES
('tts_minute', 0.10, 0.06),
('remove_watermark', 0.03, 0.00),
('priority_queue', 0.02, 0.00),
('custom_voice', 0.05, 0.02);

CREATE TABLE transactions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id UUID NOT NULL,
    amount DECIMAL(10,2),
    type VARCHAR(20), -- 'purchase', 'usage', 'subscription', 'refund'
    description TEXT,
    stripe_payment_id VARCHAR(255),
    created_at TIMESTAMPTZ DEFAULT NOW()
);

CREATE INDEX idx_transactions_account ON transactions(account_id);
CREATE INDEX idx_account_credits_subscription ON account_credits(subscription_expires);
