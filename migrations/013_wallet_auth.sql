-- wallet-based auth: polkadot sr25519 wallet login
ALTER TABLE users ADD COLUMN IF NOT EXISTS wallet_address TEXT UNIQUE;

CREATE INDEX IF NOT EXISTS idx_users_wallet ON users(wallet_address)
  WHERE wallet_address IS NOT NULL;

-- relax constraint: wallet_address is also a valid auth method
ALTER TABLE users DROP CONSTRAINT IF EXISTS auth_method_required;
ALTER TABLE users ADD CONSTRAINT auth_method_required
  CHECK (email IS NOT NULL OR public_key IS NOT NULL OR wallet_address IS NOT NULL);
