#!/bin/bash
TOKEN="B7wfKMy8vveZvrNZdVJze2Iyquzu2IzR"
response=$(curl -s -X POST "https://api.deepinfra.com/v1/inference/hexgrad/Kokoro-82M" \
  -H "Authorization: bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"text":"Hello"}')
echo "Response length: ${#response}"
echo "First 500 chars:"
echo "$response" | head -c 500
echo ""
echo ""
echo "Keys in response:"
echo "$response" | jq 'keys' 2>/dev/null || echo "Not valid JSON or jq not available"
