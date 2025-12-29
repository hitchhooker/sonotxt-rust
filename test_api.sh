#!/bin/bash
TOKEN="B7wfKMy8vveZvrNZdVJze2Iyquzu2IzR"
curl -v -X POST "https://api.deepinfra.com/v1/inference/hexgrad/Kokoro-82M" \
  -H "Authorization: bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"text":"Hello"}' 2>&1
