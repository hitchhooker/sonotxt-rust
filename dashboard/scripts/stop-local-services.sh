#!/bin/bash
systemctl --user stop minio
sudo systemctl stop valkey postgresql
echo "Services stopped"
