-- embed sites - tracks usage, not auth (auth is stateless HMAC)
create table if not exists embed_sites (
    id uuid primary key default gen_random_uuid(),
    domain text not null unique,
    -- stats
    total_requests bigint default 0,
    total_chars bigint default 0,
    last_used_at timestamptz,
    -- limits
    daily_char_limit integer default 50000,
    enabled boolean default true,
    created_at timestamptz default now()
);

create index if not exists idx_embed_sites_domain on embed_sites(domain);

-- add embed columns to jobs
alter table jobs add column if not exists embed_domain text;
alter table jobs add column if not exists user_id uuid references users(id);
