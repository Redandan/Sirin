#!/usr/bin/env bash
# Quick test of executor_open_claude with agora_staking

# This is a testing helper script
# Usage: ./test_open_claude.sh

# Note: On Windows, run this via WSL or use the PowerShell equivalent

echo "Testing Open Claude Executor with agora_staking..."
echo ""
echo "Current Sirin state:"
curl -s -X POST http://127.0.0.1:7730/mcp \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | jq '.result.tools | length'

echo ""
echo "To run the test with Open Claude executor, we need to:"
echo "1. Modify executor.rs to check for 'use_open_claude' flag"
echo "2. Call execute_test_open_claude when flag is set"
echo "3. Update MCP run_test_async to pass the flag"
echo ""
echo "For now, this is just a helper to document the testing flow."
