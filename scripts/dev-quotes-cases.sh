#!/usr/bin/env bash
# Sourced after the ordinary Anvil deposit scenario, with the trader funded and eligible.

step "A signed stream gives the trader's executable quotes; an order at the ask fills within its bound"
challenge="$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')"
signer="$(curl -sfk "$api/v1/attestation" | jq -r .enclave_address)"
[[ "$signer" =~ ^0x[0-9a-f]{40}$ ]] || fail "no enclave address in the attestation: $signer"
SIGNER_KEY="${trader_key#0x}" python3 -I "$root/scripts/dev-quotes-client.py" "$api_port" "$access_signer" 31337 "$vault" \
  "$trader" "$challenge" "$work/quote-frames" || fail "the quote client failed"

step "Every frame, quote and heartbeat, verifies under the attested key"
"$engine" verify-frames --signer "$signer" --chain-id 31337 --vault "$vault" --challenge "$challenge" \
  --path "/v1/stream?signed=1&challenge=$challenge" --frames "$work/quote-frames" || fail "the frames did not verify"
jq -se '[.[] | .msg | fromjson | select(.type == "quote")] | length > 2' "$work/quote-frames" >/dev/null \
  || fail "the frames hold too few quotes"
