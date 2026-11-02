#!/usr/bin/env bash
# Sourced after the ordinary Anvil deposit scenario; every refusal is checked in the public log.

access_expect_refusal() {
  local private_key="$1" reason="$2" kind route signed events
  for kind in order withdrawal; do
    route=/v1/orders
    [[ "$kind" == withdrawal ]] && route=/v1/withdrawals
    signed="$(access_request "$private_key" "$kind")"
    events="$(access_post "$route" "$signed")"
    jq -e --arg reason "$reason" 'any(.[]; (.type == "order_rejected" or .type == "withdrawal_rejected") and .reason.kind == "access" and .reason.reason == $reason)' \
      <<< "$events" >/dev/null || fail "expected $reason from $route: $events"
  done
}

access_funding_reference() {
  local receipt
  receipt="$(cast send "$@" --rpc-url "$rpc" --json)"
  python3 -c 'import json,sys; r=json.load(sys.stdin); v=sys.argv[1].lower(); logs=[x for x in r["logs"] if x["address"].lower()==v]; assert len(logs)==1,logs; n=logs[0]["logIndex"]; n=int(n,16) if isinstance(n,str) else n; print("%020d:%s:%s:%d" % (int(r["blockNumber"],16), r["blockHash"], r["transactionHash"], n))' "$vault" <<< "$receipt"
}

access_held_reason() { curl --max-time 2 -sk "$api/v1/access/holds/$access_reference" | jq -r '.reason // "none"'; }

access_hold() {
  local reason="$1"
  mine 3
  wait_for access_held_reason "$reason"
  expected_deposits=$((expected_deposits + 1))
}

step "An eligible client fills signed API orders; forwarding headers cannot override the socket peer"
for side in buy sell; do
  signed="$(ACCESS_ORDER_SIDE="$side" access_request "$trader_key" order)"
  events="$(access_post /v1/orders "$signed")"
  jq -e 'any(.[]; .type == "fill")' <<< "$events" >/dev/null || fail "eligible $side order did not fill: $events"
done
expected_balance="$(trader_balance)"
expected_nav="$(house_nav)"

step "US and sanctioned jurisdictions refuse authenticated orders and EVM withdrawals in the public log"
for classification in united_states sanctioned unknown; do
  access_set_fixture clear "$trader" "$classification"
  reason="$classification"
  [[ "$classification" == sanctioned ]] && reason=sanctioned_jurisdiction
  [[ "$classification" == unknown ]] && reason=jurisdiction_unknown
  access_expect_refusal "$trader_key" "$reason"
done

step "Account sanctions and unavailable, stale, unknown and forged sources all fail closed"
access_set_fixture clear "$trader"
access_expect_refusal "$trader_key" account_sanctioned
for mode in unavailable stale unknown forged; do
  access_set_fixture "$mode"
  case "$mode" in unavailable) reason=source_unavailable;; stale) reason=evidence_stale;; unknown) reason=account_unknown;; forged) reason=source_invalid;; esac
  access_expect_refusal "$trader_key" "$reason"
done
access_set_fixture clear

step "A screening outage remains pending across restart and automatically credits exactly once after recovery"
access_set_fixture unavailable
access_reference="$(access_funding_reference "$vault" 'depositFor(address,uint256)' "$trader" $((10 * usdc_units)) --private-key "$house_key")"
mine 3
access_pending_amount() { curl --max-time 2 -sk "$api/v1/access/pending/$access_reference" | jq -r '.amount // "none"'; }
access_pending_gauge() { curl --max-time 2 -sf "http://127.0.0.1:$metrics_port/metrics" | awk '$1 == "engine_access_pending_deposits" { print $2 }'; }
wait_for access_pending_amount 10
wait_for access_pending_gauge 1
expected_deposits=$((expected_deposits + 1))
[[ "$(trader_balance)" == "$expected_balance" ]] || fail "an outage deposit credited before screening"
stop_node
access_set_fixture clear
start_node
expected_balance="$(python3 -c 'from decimal import Decimal; import sys; print(Decimal(sys.argv[1]) + Decimal(10))' "$expected_balance")"
wait_for trader_balance "$expected_balance"
[[ "$(access_pending_amount)" == none ]] || fail "credited deposit remains pending"
wait_for access_pending_gauge 0
[[ "$(house_nav)" == "$expected_nav" ]] || fail "outage retry changed house NAV"
grep -Fq 'alert_resolved="access_source_failed"' "$work/node.log" || fail "screening recovery did not resolve its outage alert"

step "A never-credited signer cannot order; its deposit waits pending through a US declaration until an allowed one"
new_key="$(key 3)"
new_account="$(cast wallet address --private-key "$new_key")"
new_balance() { curl --max-time 2 -sk "$api/v1/accounts/$new_account" | jq -r '.balance // "none"'; }
signed="$(access_request "$new_key" order)"
code="$(curl --max-time 10 -sk -o /dev/null -w '%{http_code}' "$api/v1/orders" -H 'content-type: application/json' -d "$signed")"
[[ "$code" == "403" ]] || fail "a never-credited order answered $code"
access_reference="$(access_funding_reference "$vault" 'depositFor(address,uint256)' "$new_account" $((10 * usdc_units)) --private-key "$house_key")"
mine 3
wait_for access_pending_amount 10
expected_deposits=$((expected_deposits + 1))
stays new_balance none
new_class() { curl --max-time 2 -sk "$api/v1/accounts/$new_account/eligibility" | jq -r '.class // "none"'; }
access_set_fixture clear "$new_account" united_states
signed="$(access_signed_body "$new_key" eligibility "{\"account\":\"$new_account\",\"class\":\"professional\"}")"
events="$(access_post /v1/eligibility "$signed")"
jq -e 'length == 1 and .[0].type == "eligibility_rejected" and .[0].reason.kind == "access" and .[0].reason.reason == "united_states"' \
  <<< "$events" >/dev/null || fail "a declaration from a US peer was not publicly refused: $events"
[[ "$(new_class)" == none ]] || fail "a refused declaration was recorded"
stays new_balance none
[[ "$(access_pending_amount)" == 10 ]] || fail "a refused declaration released its pending deposit"
access_set_fixture clear
access_attest "$new_key"
wait_for new_balance 10
[[ "$(access_pending_amount)" == none ]] || fail "a credited deposit remains pending"

step "A fresh wallet declares and registers a session key with no deposit, and an order stays refused until a deposit credits it"
never_credited="account has never been credited"
must_declare="a never-credited account must declare first"
fresh_key="$(key 4)"
fresh_account="$(cast wallet address --private-key "$fresh_key" | tr '[:upper:]' '[:lower:]')"
session_key="$(key 5)"
session_account="$(cast wallet address --private-key "$session_key" | tr '[:upper:]' '[:lower:]')"
spare_account="$(cast wallet address --private-key "$(key 6)" | tr '[:upper:]' '[:lower:]')"
fresh_balance() { curl --max-time 2 -sk "$api/v1/accounts/$fresh_account" | jq -r '.balance // "none"'; }
fresh_nonce() { curl --max-time 2 -sk "$api/v1/accounts/$fresh_account/nonce" | jq -r .next_nonce; }
session_nonce() { curl --max-time 2 -sk "$api/v1/accounts/$session_account/nonce" | jq -r .next_nonce; }
session_expiry="$(python3 -c 'import time; print(int(time.time() * 1000) + 3600000)')"
fresh_registration() { echo "{\"account\":\"$fresh_account\",\"session_key\":\"$1\",\"expires_at\":$session_expiry}"; }
fresh_declaration="{\"account\":\"$fresh_account\",\"class\":\"professional\"}"
access_expect_forbidden /v1/orders "$(access_request "$fresh_key" order)" "$never_credited"
signed="$(access_signed_body "$fresh_key" session_key_registration "$(fresh_registration "$session_account")")"
access_expect_forbidden /v1/session-keys "$signed" "$must_declare"
[[ "$(fresh_nonce)" == 0 ]] || fail "a refused registration used the wallet's nonce"
access_attest "$fresh_key"
[[ "$(curl --max-time 2 -sk "$api/v1/accounts/$fresh_account/eligibility" | jq -r .class)" == professional ]] || fail "the declaration was not recorded"
access_expect_forbidden /v1/eligibility "$(access_signed_body "$fresh_key" eligibility "$fresh_declaration")" "account has already declared"
[[ "$(fresh_nonce)" == 1 ]] || fail "a second declaration used the wallet's nonce"
signed="$(access_signed_body "$fresh_key" session_key_registration "$(fresh_registration "$session_account")")"
events="$(access_post /v1/session-keys "$signed")"
jq -e 'length == 1 and .[0].type == "session_key_registered"' <<< "$events" >/dev/null || fail "the registration did not apply: $events"
# Revocation is the wallet's too before its first deposit: a second key comes and goes.
signed="$(access_signed_body "$fresh_key" session_key_registration "$(fresh_registration "$spare_account")")"
events="$(access_post /v1/session-keys "$signed")"
jq -e '.[0].type == "session_key_registered"' <<< "$events" >/dev/null || fail "the second registration did not apply: $events"
signed="$(access_signed_body "$fresh_key" session_key_revocation "{\"account\":\"$fresh_account\",\"session_key\":\"$spare_account\"}")"
events="$(curl --max-time 10 -skf -X DELETE "$api/v1/session-keys" -H 'content-type: application/json' -d "$signed")"
jq -e 'length == 1 and .[0].type == "session_key_revoked"' <<< "$events" >/dev/null || fail "the revocation did not apply: $events"
access_expect_forbidden /v1/orders "$(access_request "$fresh_key" order)" "$never_credited"
access_expect_forbidden /v1/orders "$(access_session_order "$session_key" "$fresh_account")" "$never_credited"
[[ "$(session_nonce)" == 0 ]] || fail "a refused order used the session key's nonce"
# What the console reads for a wallet with no account: its history and nonces answer, its account does not.
fresh_history() { curl --max-time 2 -sk "$api/v1/accounts/$fresh_account/history" | jq -r '[.events[]?.type] | unique | join(",")'; }
wait_for fresh_history "eligibility_attested,session_key_registered,session_key_revoked"
[[ "$(fresh_balance)" == none ]] || fail "a wallet with no deposit has an account"

step "A deposit after the declaration credits at its first screening, and the session key fills orders with no further account signature, across a restart"
fresh_nonce_before="$(fresh_nonce)"
access_reference="$(access_funding_reference "$vault" 'depositFor(address,uint256)' "$fresh_account" $((1000 * usdc_units)) --private-key "$house_key")"
fresh_reference="$access_reference"
mine 3
wait_for fresh_balance 1000
expected_deposits=$((expected_deposits + 1))
events="$(access_post /v1/orders "$(access_session_order "$session_key" "$fresh_account")")"
jq -e 'any(.[]; .type == "fill")' <<< "$events" >/dev/null || fail "the session key's order did not fill: $events"
stop_node
start_node
[[ "$(curl --max-time 2 -sk "$api/v1/accounts/$fresh_account/eligibility" | jq -r .class)" == professional ]] || fail "the declaration did not survive the restart"
# The feed needs a moment after a restart; a refused attempt uses a session nonce and no account signature.
session_sell_fills() {
  local events
  events="$(access_post /v1/orders "$(ACCESS_ORDER_SIDE=sell access_session_order "$session_key" "$fresh_account")" || true)"
  jq -e 'any(.[]?; .type == "fill")' <<< "$events" >/dev/null 2>&1 && echo filled || echo unfilled
}
wait_for session_sell_fills filled
[[ "$(fresh_nonce)" == "$fresh_nonce_before" ]] || fail "the session key's orders used the wallet's nonce"
[[ "$(session_nonce)" -ge 2 ]] || fail "the session key's orders did not use its own nonces"
expected_nav="$(house_nav)"

step "A sanctioned payer cannot fund an eligible beneficiary or the house by direct vault calls"
payer="$(cast wallet address --private-key "$house_key")"
access_set_fixture clear "$payer"
access_reference="$(access_funding_reference "$vault" 'depositFor(address,uint256)' "$trader" $((10 * usdc_units)) --private-key "$house_key")"
access_hold payer_sanctioned
access_reference="$(access_funding_reference "$vault" 'depositToHouse(uint256)' $((10 * usdc_units)) --private-key "$house_key")"
access_hold payer_sanctioned

step "Contract-wallet funding is screened as the wallet, independently of its transaction sender"
wallet="$(cd "$root/contracts" && create test/mocks/MockWallet.sol:MockWallet --constructor-args "$trader")"
send "$usdc" 'mint(address,uint256)' "$wallet" $((10 * usdc_units)) --private-key "$deployer_key"
access_set_fixture clear "$wallet"
access_reference="$(access_funding_reference "$wallet" 'fund(address,uint256)' "$vault" $((10 * usdc_units)) --private-key "$deployer_key")"
access_hold payer_sanctioned
[[ "$(trader_balance)" == "$expected_balance" ]] || fail "sanctioned payer changed trader balance"
[[ "$(house_nav)" == "$expected_nav" ]] || fail "held collateral changed house NAV"
# Counters are per process, so the held-deposit count is read before the restart below.
metrics="$(curl --max-time 2 -sf "http://127.0.0.1:$metrics_port/metrics")"
grep -Fq 'engine_rejections_total{event="deposit_held",reason="access"}' <<< "$metrics" || fail "held-deposit metric missing"

step "Sanctioned holds remain uncredited after source recovery, a restart and loss of the local cursor"
access_set_fixture clear
stop_node
rm "$work/custody-cursor.json"
start_node
wait_for rescanned_deposits "$expected_deposits"
[[ "$(access_held_reason)" == payer_sanctioned ]] || fail "a sanctioned hold was released"
[[ "$(trader_balance)" == "$expected_balance" ]] || fail "restart credited a sanctioned hold"
[[ "$(house_nav)" == "$expected_nav" ]] || fail "restart changed held collateral accounting"
grep -Fq 'alert="access_deposit_held"' "$work/node.log" || fail "held collateral has no operator alert"
jq -se '
  [ .[] | select(.entry == "command") | .events[]? | select(.type == "order_rejected" or .type == "withdrawal_rejected") | .reason.reason ] as $reasons |
  ["united_states","sanctioned_jurisdiction","jurisdiction_unknown","account_sanctioned","source_unavailable","evidence_stale","account_unknown","source_invalid"] |
  all(.[]; . as $required | $reasons | index($required) != null)
' "$work"/log/[0-9]*.jsonl >/dev/null || fail "an API access refusal is missing from the public log"
jq -se --arg account "$new_account" '
  any(.[] | select(.entry == "command") | .events[]?; .type == "eligibility_rejected" and .account == ($account | ascii_downcase) and .reason.reason == "united_states")
' "$work"/log/[0-9]*.jsonl >/dev/null || fail "the refused US declaration is missing from the public log"
jq -se --arg account "$fresh_account" --arg reference "$fresh_reference" '
  [ .[] | select(.entry == "command") ] as $commands |
  ($commands | map(.events // [] | any(.type == "session_key_registered" and .account == $account)) | index(true)) as $registered |
  ($commands | map(.cmd.type == "access_custody_deposit" and .cmd.receipt.beneficiary.account == $account) | index(true)) as $deposited |
  $registered != null and $deposited != null and $registered < $deposited and
  ($commands | any(.cmd.type == "retry_deposit_screening" and .cmd.chain_ref == $reference) | not) and
  ($commands | any(.events[]?; .type == "deposit_pending" and .chain_ref == $reference) | not)
' "$work"/log/[0-9]*.jsonl >/dev/null || fail "the wallet did not register before its deposit, or the deposit needed a retry"
if grep -Eq '127\.0\.0\.1|203\.0\.113\.99' "$work"/log/[0-9]*.jsonl; then fail "an IP address leaked into the public log"; fi
echo "PASS: signed, screened eligibility; setup before a first deposit; API access refusals; direct payer and contract-wallet holds survive restart"
