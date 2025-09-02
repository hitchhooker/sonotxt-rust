#!/bin/bash
set -e

# Install packages
sudo pacman -S --needed postgresql minio minio-client

# Check for redis/valkey
if pacman -Qi redis &>/dev/null; then
   REDIS_SERVICE="redis"
elif pacman -Qi valkey &>/dev/null; then
   REDIS_SERVICE="valkey"
else
   sudo pacman -S --needed valkey
   REDIS_SERVICE="valkey"
fi

# PostgreSQL setup
sudo -u postgres initdb -D /var/lib/postgres/data 2>/dev/null || true
sudo systemctl start postgresql
sudo -u postgres psql << SQL
DROP DATABASE IF EXISTS sonotxt;
DROP USER IF EXISTS sonotxt;
CREATE USER sonotxt WITH PASSWORD 'sonotxt';
CREATE DATABASE sonotxt OWNER sonotxt;
SQL

# Run migrations if they exist
if [ -d "./migrations" ]; then
   for f in ./migrations/*.sql; do
       [ -e "$f" ] && sudo -u postgres psql -d sonotxt < "$f"
   done
fi

# Redis/Valkey
if [ "$REDIS_SERVICE" = "redis" ] && ! systemctl list-unit-files | grep -q "^redis.service"; then
    REDIS_SERVICE="valkey"
fi
sudo systemctl start $REDIS_SERVICE

# MinIO
mkdir -p ~/.minio/data
cat > ~/.config/systemd/user/minio.service << 'SVC'
[Unit]
Description=MinIO
After=network.target

[Service]
Type=simple
Environment="MINIO_ROOT_USER=minioadmin"
Environment="MINIO_ROOT_PASSWORD=minioadmin"
ExecStart=/usr/bin/minio server %h/.minio/data --console-address ":9001"
Restart=on-failure

[Install]
WantedBy=default.target
SVC

systemctl --user daemon-reload
systemctl --user start minio

# Wait for MinIO and create bucket
sleep 5
mcli config host add myminio http://localhost:9000 minioadmin minioadmin
mcli mb myminio/sonotxt-audio 2>/dev/null || true
mcli anonymous set public myminio/sonotxt-audio

echo "Services running on standard ports: PostgreSQL (5432), $REDIS_SERVICE (6379), MinIO (9000/9001)"
