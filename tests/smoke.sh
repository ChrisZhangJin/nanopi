#!/usr/bin/env bash
# nanopi v0.5 smoke test.
#
# Exercises the 11 v0.5 acceptance criteria from docs/v0.5-research.md §8.5.
# Requires OPENAI_API_KEY, OPENAI_BASE_URL, OPENAI_MODEL env vars.
#
# Usage:
#   export OPENAI_API_KEY=sk-...
#   export OPENAI_BASE_URL=https://api.deepseek.com/v1
#   export OPENAI_MODEL=deepseek-v4-flash
#   ./tests/smoke.sh

set -euo pipefail

BIN="${BIN:-./target/x86_64-unknown-linux-musl/release/nanopi}"
KEY="${OPENAI_API_KEY:?OPENAI_API_KEY is required}"
BASE="${OPENAI_BASE_URL:-https://api.deepseek.com/v1}"
MODEL="${OPENAI_MODEL:-deepseek-v4-flash}"

pass() { echo "✓ $1"; }
fail() { echo "✗ $1"; exit 1; }
require_cmd() { command -v "$1" >/dev/null 2>&1 || fail "missing dependency: $1"; }

require_cmd jq
[ -x "$BIN" ] || fail "binary not found at $BIN (run cargo build first)"

# Test 0: --help works.
echo "=== Test 0: --help ==="
$BIN --help >/dev/null || fail "--help failed"
pass "--help works"

# Test 1: simple Q&A (no tools).
echo "=== Test 1: simple Q&A in -p mode ==="
out=$($BIN -p --yolo --model "$MODEL" --base-url "$BASE" --api-key "$KEY" "2+2=?")
echo "$out" | grep -q '4' || fail "expected '4' in output"
pass "simple Q&A returns 4"

# Test 2: -p --output json schema.
echo "=== Test 2: -p --output json ==="
tmp=$(mktemp)
$BIN -p --output json --yolo --model "$MODEL" --base-url "$BASE" --api-key "$KEY" \
    "in one word: yes or no?" > "$tmp"
jq -e '.session_id' "$tmp" >/dev/null || fail "missing session_id in JSON output"
jq -e '.model' "$tmp" >/dev/null || fail "missing model in JSON output"
jq -e '.duration_ms' "$tmp" >/dev/null || fail "missing duration_ms in JSON output"
jq -e '.finish_reason' "$tmp" >/dev/null || fail "missing finish_reason in JSON output"
jq -e '.messages | length > 0' "$tmp" >/dev/null || fail "messages must be non-empty"
jq -e '.usage' "$tmp" >/dev/null || fail "missing usage in JSON output"
pass "JSON envelope has all required fields"
rm -f "$tmp"

# Test 3: tool use (read).
echo "=== Test 3: read tool ==="
tmp=$(mktemp)
$BIN -p --yolo --model "$MODEL" --base-url "$BASE" --api-key "$KEY" \
    "read /etc/hostname and tell me what you see, verbatim" > "$tmp" 2>&1
if ! grep -q -i 'hostname\|host' "$tmp"; then
    cat "$tmp"
    fail "read tool did not surface hostname in output"
fi
# Read session JSONL: should contain a tool_call entry.
latest=$(ls -t ~/.nanopi/sessions/*.jsonl 2>/dev/null | head -1)
[ -n "$latest" ] || fail "no session file written"
if ! grep -q 'tool_name' "$latest"; then
    fail "no tool_call entry in session $latest"
fi
pass "read tool invoked and recorded in session"
rm -f "$tmp"

# Test 4: tool use (write).
echo "=== Test 4: write tool ==="
tmpfile=$(mktemp -u)/nanopi-smoke-$$.txt
mkdir -p "$(dirname "$tmpfile")"
content="hello from nanopi smoke test $(date +%s)"
$BIN -p --yolo --model "$MODEL" --base-url "$BASE" --api-key "$KEY" \
    "create file $tmpfile with the text: $content" >/dev/null 2>&1
if [ -f "$tmpfile" ] && grep -q "$content" "$tmpfile"; then
    pass "write tool created $tmpfile"
else
    fail "write tool did not create $tmpfile"
fi
rm -f "$tmpfile"

# Test 5: bash tool (with the configured model, may need simple commands).
echo "=== Test 5: bash tool ==="
tmp=$(mktemp)
out=$($BIN -p --yolo --model "$MODEL" --base-url "$BASE" --api-key "$KEY" \
    "run 'echo hello-from-bash' and tell me the output" 2>&1)
echo "$out" | grep -q "hello-from-bash" || fail "bash tool did not produce expected output"
pass "bash tool produced expected output"

# Test 6: yolo flag accepted and prints warning.
echo "=== Test 6: yolo warning ==="
out=$($BIN -p --yolo --model "$MODEL" --base-url "$BASE" --api-key "$KEY" "hi" 2>&1)
echo "$out" | grep -q -i "yolo\|skipping" || fail "expected yolo warning to stderr"
pass "yolo warning printed"

# Test 7: --no-hooks flag accepted.
echo "=== Test 7: --no-hooks flag ==="
out=$($BIN -p --no-hooks --yolo --model "$MODEL" --base-url "$BASE" --api-key "$KEY" "hi" 2>&1)
[ -n "$out" ] || fail "--no-hooks invocation produced no output"
pass "--no-hooks flag accepted"

# Test 8: invalid model gives exit code 1 or 2, not 0.
echo "=== Test 8: invalid model errors ==="
out=$($BIN -p --yolo --model "nonexistent-model-xyz" --base-url "$BASE" --api-key "$KEY" "hi" 2>&1) || code=$?
code=${code:-0}
[ "$code" -ne 0 ] || fail "expected non-zero exit code on invalid model, got 0"
pass "invalid model returns non-zero exit code ($code)"

# Test 9: stdin message in interactive mode (single-shot).
echo "=== Test 9: stdin message in interactive mode ==="
out=$(echo "what is 2+2? one number" | $BIN --yolo --model "$MODEL" --base-url "$BASE" --api-key "$KEY" 2>&1)
echo "$out" | grep -q '4' || fail "stdin-driven turn did not produce 4"
pass "stdin-driven turn works"

# Test 10: --output json contains messages array with user/assistant entries.
echo "=== Test 10: JSON messages array ==="
tmp=$(mktemp)
$BIN -p --output json --yolo --model "$MODEL" --base-url "$BASE" --api-key "$KEY" \
    "say hi" > "$tmp"
roles=$(jq -r '.messages[].role' "$tmp")
echo "$roles" | grep -q 'user' || fail "messages missing user role"
echo "$roles" | grep -q 'assistant' || fail "messages missing assistant role"
pass "JSON messages array has user + assistant"
rm -f "$tmp"

# Test 11: binary is statically linked.
echo "=== Test 11: static linking ==="
file "$BIN" | grep -q 'static' || fail "binary is not statically linked"
pass "binary is statically linked"

echo
echo "All smoke tests passed."