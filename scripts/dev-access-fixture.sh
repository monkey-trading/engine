#!/usr/bin/env bash
# Sourced by local Anvil scenarios. The source signs synthetic classifications with Anvil key 0.

build_access_tools() {
  cargo build -q --locked -p engine-node --example access_source --manifest-path "$root/Cargo.toml"
  cargo build -q --locked -p engine-core --example sign_request --manifest-path "$root/Cargo.toml"
  access_target="$(cargo metadata --format-version 1 --no-deps --manifest-path "$root/Cargo.toml" | jq -r .target_directory)"
  access_signer="$access_target/debug/examples/sign_request"
}

start_access_fixture() {
  local port="${ACCESS_SOURCE_PORT:-18846}"
  printf '%s\n' '{"mode":"clear","jurisdictions":{},"sanctioned":[]}' > "$work/access-fixture.json"
  "$access_target/debug/examples/access_source" "127.0.0.1:$port" 31337 "$vault" "$work/access-fixture.json" \
    > "$work/access-source.log" 2>&1 &
  access_fixture_pid=$!
  for _ in $(seq 1 100); do
    kill -0 "$access_fixture_pid" 2>/dev/null || fail "access fixture source exited"
    if curl --max-time 2 -sf "http://127.0.0.1:$port" -H 'content-type: application/json' \
      -d '{"request_hash":"0x0000000000000000000000000000000000000000000000000000000000000000","account":null,"payer":null,"peer":null}' >/dev/null; then
      return 0
    fi
    sleep 0.1
  done
  fail "access fixture source did not start"
}

write_access_config() {
  cat >> "$1" <<EOF

[access]
source = "$(cast wallet address --private-key "$deployer_key")"
endpoint = "http://127.0.0.1:${ACCESS_SOURCE_PORT:-18846}"
timeout = "2s"
max_age = "30s"
max_response_bytes = 4096
max_in_flight = 8
EOF
}

access_signed_body() {
  local private_key="$1" kind="$2" body="$3" account nonce expires_at
  account="$(jq -r .account <<< "$body")"
  nonce="$(curl --max-time 5 -skf "$api/v1/accounts/$account/nonce" | jq -er .next_nonce)"
  expires_at="$(python3 -c 'import time; print(int(time.time() * 1000) + 60000)')"
  SIGNER_KEY="${private_key#0x}" "$access_signer" 31337 "$vault" "$nonce" "$expires_at" "$kind" "$body"
}

access_post() {
  curl --max-time 10 -skf "$api$1" -H 'content-type: application/json' \
    -H 'x-forwarded-for: 203.0.113.99' -H 'forwarded: for=203.0.113.99' -d "$2"
}

access_attest() {
  local private_key="$1" account signed events
  account="$(cast wallet address --private-key "$private_key")"
  for _ in $(seq 1 150); do
    if [[ "$(curl --max-time 2 -sk "$api/v1/accounts/$account/eligibility" | jq -r '.class // "none"')" == professional ]]; then
      return 0
    fi
    signed="$(access_signed_body "$private_key" eligibility "{\"account\":\"$account\",\"class\":\"professional\"}")"
    events="$(access_post /v1/eligibility "$signed" || true)"
    if jq -e 'any(.[]?; .type == "eligibility_attested")' <<< "$events" >/dev/null 2>&1; then return 0; fi
    sleep 0.2
  done
  fail "signed eligibility declaration was not recorded"
}

access_request() {
  local private_key="$1" kind="$2" account body mark
  account="$(cast wallet address --private-key "$private_key")"
  if [[ "$kind" == withdrawal ]]; then
    body="{\"account\":\"$account\",\"amount\":\"1\"}"
  else
    mark="$(curl --max-time 5 -skf "$api/v1/markets" | jq -er '.[] | select(.id == 1) | .mark')"
    body="{\"account\":\"$account\",\"client_id\":46,\"market\":1,\"side\":\"${ACCESS_ORDER_SIDE:-buy}\",\"qty\":\"0.01\",\"kind\":{\"kind\":\"market\",\"quoted\":\"$mark\"}}"
  fi
  access_signed_body "$private_key" "$kind" "$body"
}

access_set_fixture() {
  python3 - "$work/access-fixture.json" "$1" "${2:-}" "${3:-}" <<'PY'
import json, os, sys
path, mode, account, jurisdiction = sys.argv[1:]
fixture = {"mode": mode, "jurisdictions": {}, "sanctioned": []}
if jurisdiction:
    fixture["jurisdictions"][account.lower()] = jurisdiction
elif account:
    fixture["sanctioned"].append(account.lower())
with open(path + ".new", "w") as out:
    json.dump(fixture, out)
os.replace(path + ".new", path)
PY
}
