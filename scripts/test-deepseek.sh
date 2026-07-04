#!/usr/bin/env bash
# =============================================================================
# DeepSeek API Connectivity Test
# =============================================================================
# Standalone script — NOT wired into the main app. This is a one-shot smoke
# test to prove the DEEPSEEK_API_KEY from .env yields a real, working response.
#
# Usage:
#   1. Ensure .env exists at the project root with DEEPSEEK_API_KEY set.
#   2. Run: ./scripts/test-deepseek.sh
#
# The script prints the HTTP status code and the model's response text.
# It never prints, logs, or commits the API key itself.
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ENV_FILE="$PROJECT_ROOT/.env"

if [ ! -f "$ENV_FILE" ]; then
    echo "ERROR: .env file not found at $ENV_FILE"
    echo "Create it from .env.example and add your DEEPSEEK_API_KEY."
    exit 1
fi

# shellcheck disable=SC1090
source "$ENV_FILE"

if [ -z "${DEEPSEEK_API_KEY:-}" ]; then
    echo "ERROR: DEEPSEEK_API_KEY is not set in .env"
    exit 1
fi

echo "=== DeepSeek API Connectivity Test ==="
echo "Endpoint: POST https://api.deepseek.com/chat/completions"
echo "Model:    deepseek-v4-flash"
echo "Prompt:   \"Reply with exactly the word: pong\""
echo ""

RESPONSE=$(curl -s -w "\n%{http_code}" \
    "https://api.deepseek.com/chat/completions" \
    -H "Content-Type: application/json" \
    -H "Authorization: Bearer ${DEEPSEEK_API_KEY}" \
    -d '{
        "model": "deepseek-v4-flash",
        "messages": [
            {"role": "user", "content": "Reply with exactly the word: pong"}
        ],
        "stream": false
    }')

HTTP_CODE=$(echo "$RESPONSE" | tail -1)
BODY=$(echo "$RESPONSE" | sed '$d')

echo "HTTP Status: $HTTP_CODE"
echo ""
echo "Raw Response:"
echo "$BODY"
echo ""

if [ "$HTTP_CODE" = "200" ]; then
    # Extract the model's reply from the JSON response using grep/sed
    REPLY=$(echo "$BODY" | sed -n 's/.*"content":"\([^"]*\)".*/\1/p')
    echo "=== RESULT ==="
    echo "SUCCESS: API call succeeded."
    echo "Model reply: \"$REPLY\""
else
    echo "=== RESULT ==="
    echo "FAILURE: API call returned HTTP $HTTP_CODE."
    echo "Check your API key and network connectivity."
fi
