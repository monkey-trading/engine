#!/usr/bin/env bash
# Dev scenario for the API's public certificate, against Pebble, Let's Encrypt's ACME test CA, on
# loopback; nothing reaches a public CA:
#   1. download Pebble and its challenge test server, pinned by SHA-256, under image/retry;
#   2. make a throwaway CA for Pebble's own HTTPS listener, which the node trusts through
#      [api.tls.acme.test_ca], and start the test server's DNS mapping the test domain to loopback;
#   3. start Pebble issuing 60-second certificates and validating TLS-ALPN-01 on the node's API port,
#      then run a node on config/dev.toml's mocks with [api.tls.acme] pointed at it;
#   4. wait for the certificate, then for its renewal at two thirds of its life, which reuses the
#      first order's validation, then stop Pebble and wait for the next renewal to fail;
#   5. build the production binary and run it on a config with real providers and no [api.tls.acme].
# Passes when the served chain verifies against Pebble's root for the domain and its key hashes to
# the attestation's tls_public_key_sha256, both before and after a renewal without a restart; the
# metrics gauge holds the served certificate's notAfter; the failed renewal raises
# certificate_expiring while the renewed certificate keeps serving; and the production binary
# refuses the config for its self-signed certificate.
#
# Needs curl, jq, and openssl. API_PORT, METRICS_PORT, PEBBLE_PORT, PEBBLE_MANAGEMENT_PORT,
# DNS_PORT, and CHALLTESTSRV_PORT override 18443, 19473, 14000, 15000, 18053, and 18055.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
api_port="${API_PORT:-18443}"
metrics_port="${METRICS_PORT:-19473}"
pebble_port="${PEBBLE_PORT:-14000}"
pebble_management_port="${PEBBLE_MANAGEMENT_PORT:-15000}"
dns_port="${DNS_PORT:-18053}"
challtestsrv_port="${CHALLTESTSRV_PORT:-18055}"
domain="api.engine.test"
pebble_version="v2.10.1"
node_pid=""
pebble_pid=""
challtestsrv_pid=""

cleanup() {
  for pid in "$node_pid" "$pebble_pid" "$challtestsrv_pid"; do
    if [[ -n "$pid" ]]; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  rm -rf "$work"
}
trap cleanup EXIT

step() { echo "==> $*"; }
fail() {
  echo "FAIL: $*" >&2
  for log in node pebble; do
    [[ -f "$work/$log.log" ]] && { echo "--- $log.log" >&2; tail -n 40 "$work/$log.log" >&2; }
  done
  exit 1
}

sha256() { if command -v sha256sum > /dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1; }

# The release asset's SHA-256, from the release's published digests.
asset_sha256() {
  case "$1" in
    pebble-linux-amd64) echo 4f2fcb5bca8c85c9cf73ad140fccfc0d2be40bd81ab99879c79b7b8a0b4f70ed ;;
    pebble-linux-arm64) echo b53fd072a69eb7692451de4e8b0667e0bdf5cccd7e36fc51b8eaf2fcc135ed9f ;;
    pebble-darwin-amd64) echo e670ff869886022637e077502a62e7f23be693c45a5a6727ebd76da8fdce64dc ;;
    pebble-darwin-arm64) echo 09a3a4e6ebed71e8d83294a26d361232262f45a7488f5de7bccb5887b395217f ;;
    pebble-challtestsrv-linux-amd64) echo e93a5aa25ecdf3af2f9fbb2de32b0173e64a2eae81002a4ccfe35fa6f4f60b92 ;;
    pebble-challtestsrv-linux-arm64) echo db8e1a79ccdb2195c489fbe4f40fddb7f30e86f9cd8a07912566ee5025094d6c ;;
    pebble-challtestsrv-darwin-amd64) echo 796bd923f2c595dd7bf15ae693096abfb1df962cb3673c7981ff306daa5c4a52 ;;
    pebble-challtestsrv-darwin-arm64) echo 59bf917fe39c96e2edca980fc2899f4f04aa1ce5485f28d419d18237b902cf82 ;;
    *) fail "no pinned digest for $1" ;;
  esac
}

# Downloads release binary `$1` for this platform into $work/bin and checks its digest.
fetch() {
  local asset="$1-$os-$arch"
  mkdir -p "$work/download/$asset" "$work/bin"
  "$root/image/retry" curl -fsSL --connect-timeout 30 --speed-limit 1000 --speed-time 60 \
    -o "$work/download/$asset.tar.gz" \
    "https://github.com/letsencrypt/pebble/releases/download/$pebble_version/$asset.tar.gz"
  [[ "$(sha256 "$work/download/$asset.tar.gz")" == "$(asset_sha256 "$asset")" ]] \
    || fail "$asset.tar.gz does not match its pinned SHA-256"
  tar -xzf "$work/download/$asset.tar.gz" -C "$work/download/$asset"
  # The release archives hold the binaries without their execute bit.
  install -m 0755 "$work/download/$asset/$asset/$os/$arch/$1" "$work/bin/$1"
}

# The served chain as `openssl s_client` prints it for a handshake naming the domain, when it
# verifies against Pebble's root for the domain; nothing otherwise, as for the self-signed certificate.
served() {
  local out
  out="$(openssl s_client -connect "127.0.0.1:$api_port" -servername "$domain" -showcerts \
    -CAfile "$work/pebble-root.pem" -verify_return_error -verify_hostname "$domain" < /dev/null 2> /dev/null)" || true
  if grep -q "Verify return code: 0 (ok)" <<< "$out"; then printf '%s\n' "$out"; fi
}
served_leaf() { served | openssl x509 2> /dev/null; }
leaf_serial() { served_leaf | openssl x509 -noout -serial 2> /dev/null | cut -d= -f2 || true; }
# SHA-256 of the served leaf's DER SubjectPublicKeyInfo, as 0x hex.
leaf_key_sha256() {
  echo "0x$(served_leaf | openssl x509 -noout -pubkey | openssl pkey -pubin -outform DER | openssl dgst -sha256 -r | cut -d' ' -f1)"
}
attested_key_sha256() {
  curl -sf --cacert "$work/pebble-root.pem" --resolve "$domain:$api_port:127.0.0.1" \
    "https://$domain:$api_port/v1/attestation" | jq -r .tls_public_key_sha256
}
metric() {
  curl -sf "http://127.0.0.1:$metrics_port/metrics" | awk -v name="$1" '$1 == name { print $2 }' || true
}

# Polls until `$1` prints something other than `$2`, for up to `$3` seconds, and prints it.
wait_change() {
  local got=""
  for _ in $(seq 1 "$(( $3 * 2 ))"); do
    got="$($1)"
    if [[ -n "$got" && "$got" != "$2" ]]; then echo "$got"; return 0; fi
    kill -0 "$node_pid" 2>/dev/null || fail "the node exited"
    sleep 0.5
  done
  fail "$1 still printed '$got' after $3 seconds"
}

case "$(uname -s)" in Linux) os=linux ;; Darwin) os=darwin ;; *) fail "unsupported OS $(uname -s)" ;; esac
case "$(uname -m)" in x86_64 | amd64) arch=amd64 ;; arm64 | aarch64) arch=arm64 ;; *) fail "unsupported CPU $(uname -m)" ;; esac

step "download Pebble $pebble_version for $os-$arch"
fetch pebble
fetch pebble-challtestsrv

step "build the node"
(cd "$root" && cargo build -p engine-node --locked --quiet)
target="$(cargo metadata --format-version 1 --no-deps --manifest-path "$root/Cargo.toml" | jq -r .target_directory)"
engine="$target/debug/engine"

step "make a throwaway CA for Pebble's own HTTPS listener"
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 1 -subj /CN=dev-acme-listener-ca \
  -addext basicConstraints=critical,CA:TRUE -addext keyUsage=critical,keyCertSign \
  -keyout "$work/listener-ca.key" -out "$work/listener-ca.pem" 2> /dev/null
openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -subj /CN=127.0.0.1 \
  -keyout "$work/listener.key" -out "$work/listener.csr" 2> /dev/null
printf 'subjectAltName=IP:127.0.0.1\nbasicConstraints=CA:FALSE\nkeyUsage=digitalSignature\nextendedKeyUsage=serverAuth\n' \
  > "$work/listener.ext"
openssl x509 -req -in "$work/listener.csr" -CA "$work/listener-ca.pem" -CAkey "$work/listener-ca.key" \
  -CAcreateserial -days 1 -extfile "$work/listener.ext" -out "$work/listener.pem" 2> /dev/null

step "map $domain to 127.0.0.1 in the challenge test server's DNS on 127.0.0.1:$dns_port"
"$work/bin/pebble-challtestsrv" -management "127.0.0.1:$challtestsrv_port" -dnsserver "127.0.0.1:$dns_port" \
  -http01 "" -https01 "" -tlsalpn01 "" -defaultIPv4 "" -defaultIPv6 "" > "$work/challtestsrv.log" 2>&1 &
challtestsrv_pid=$!
for _ in $(seq 1 100); do
  curl -sf -d "{\"host\":\"$domain\",\"addresses\":[\"127.0.0.1\"]}" "http://127.0.0.1:$challtestsrv_port/add-a" \
    > /dev/null && break
  sleep 0.1
done
curl -sf -d "{\"host\":\"$domain\",\"addresses\":[\"127.0.0.1\"]}" "http://127.0.0.1:$challtestsrv_port/add-a" \
  > /dev/null || fail "the challenge test server did not start"

step "start Pebble on 127.0.0.1:$pebble_port, validating TLS-ALPN-01 on port $api_port"
cat > "$work/pebble.json" <<JSON
{
  "pebble": {
    "listenAddress": "127.0.0.1:$pebble_port",
    "managementListenAddress": "127.0.0.1:$pebble_management_port",
    "certificate": "$work/listener.pem",
    "privateKey": "$work/listener.key",
    "httpPort": 1,
    "tlsPort": $api_port,
    "ocspResponderURL": "",
    "externalAccountBindingRequired": false,
    "domainBlocklist": [],
    "retryAfter": { "authz": 1, "order": 1 },
    "keyAlgorithm": "ecdsa",
    "profiles": { "default": { "description": "short-lived, so the node renews", "validityPeriod": 60 } }
  }
}
JSON
# No validation delay and no rejected nonces. Pebble reuses a valid authorization for the same
# account, so the first order is validated over TLS-ALPN-01 and the renewal reuses it.
PEBBLE_VA_NOSLEEP=1 PEBBLE_WFE_NONCEREJECT=0 PEBBLE_AUTHZREUSE=100 \
  "$work/bin/pebble" -config "$work/pebble.json" -dnsserver "127.0.0.1:$dns_port" -strict > "$work/pebble.log" 2>&1 &
pebble_pid=$!
for _ in $(seq 1 100); do
  curl -sf --cacert "$work/listener-ca.pem" "https://127.0.0.1:$pebble_port/dir" > /dev/null && break
  sleep 0.1
done
curl -sf --cacert "$work/listener-ca.pem" "https://127.0.0.1:$pebble_management_port/roots/0" \
  > "$work/pebble-root.pem" || fail "Pebble did not start"

step "write the node config"
sed -e "s|^listen = \"127.0.0.1:8080\"|listen = \"127.0.0.1:$api_port\"|" \
  -e "s|^listen = \"127.0.0.1:9464\"|listen = \"127.0.0.1:$metrics_port\"|" \
  -e "s|^log_dir = .*|log_dir = \"$work/log\"|" \
  -e "s|^dir = \"data/dev/history\"|dir = \"$work/history\"|" \
  -e "s|dir = \"data/dev/alerts\"|dir = \"$work/alerts\"|" \
  "$root/config/dev.toml" > "$work/node.toml"
cat >> "$work/node.toml" <<TOML

[api.tls.acme]
domain = "$domain"
directory_url = "https://127.0.0.1:$pebble_port/dir"
contact = "mailto:operator@engine.test"

[api.tls.acme.test_ca]
root = '''
$(cat "$work/listener-ca.pem")
'''
TOML
for edit in ":$api_port\"" ":$metrics_port\"" "$work/log\"" "$work/history\"" "$work/alerts\""; do
  grep -qF "$edit" "$work/node.toml" || fail "config edit missing: $edit"
done

step "run the node"
"$engine" run --config "$work/node.toml" > "$work/node.log" 2>&1 &
node_pid=$!

step "wait for the CA certificate"
first="$(wait_change leaf_serial "" 60)"
attested="$(attested_key_sha256)"
[[ "$attested" =~ ^0x[0-9a-f]{64}$ ]] || fail "no attested TLS key hash: '$attested'"
[[ "$(leaf_key_sha256)" == "$attested" ]] || fail "the served key $(leaf_key_sha256) is not the attested $attested"
grep -q 'alert="certificate_pending"' "$work/node.log" || fail "certificate_pending was not raised before the first certificate"
not_after="$(served_leaf | openssl x509 -noout -enddate | cut -d= -f2 | tr -s ' ' \
  | jq -R 'strptime("%b %d %H:%M:%S %Y GMT") | mktime')"
gauge="$(metric engine_tls_certificate_not_after_seconds)"
awk -v gauge="$gauge" -v not_after="$not_after" 'BEGIN { exit !(gauge != "" && gauge + 0 == not_after + 0) }' \
  || fail "the gauge holds '$gauge', the certificate's notAfter is $not_after"
echo "issued serial $first for the attested key $attested"

step "wait for the renewal at two thirds of the certificate's 60 seconds"
renewed="$(wait_change leaf_serial "$first" 60)"
[[ "$(leaf_key_sha256)" == "$attested" ]] || fail "the renewed certificate is for $(leaf_key_sha256), not $attested"
[[ "$(attested_key_sha256)" == "$attested" ]] || fail "the attestation changed: the node restarted"
[[ "$(grep -c 'certificate ordered' "$work/node.log")" == 2 ]] || fail "expected two orders, the first and the renewal"
[[ "$(grep -c 'answering the tls-alpn-01 challenge' "$work/node.log")" == 1 ]] \
  || fail "expected one TLS-ALPN-01 validation, for the first order; the renewal's order is ready at once"
kill -0 "$node_pid" 2>/dev/null || fail "the node exited"
echo "renewed as serial $renewed without a restart"

step "stop Pebble, and wait for the next renewal to fail"
kill "$pebble_pid"
wait "$pebble_pid" 2>/dev/null || true
pebble_pid=""
for _ in $(seq 1 120); do
  grep -q 'alert="certificate_expiring"' "$work/node.log" && break
  kill -0 "$node_pid" 2>/dev/null || fail "the node exited"
  sleep 0.5
done
grep -q 'alert="certificate_expiring"' "$work/node.log" || fail "a failed renewal did not raise certificate_expiring"
[[ "$(leaf_serial)" == "$renewed" ]] || fail "the node stopped serving the renewed certificate"

step "stop the node"
kill -TERM "$node_pid"
wait "$node_pid" || fail "node exited with an error"
node_pid=""

step "build the production binary and run it on a config without [api.tls.acme]"
(cd "$root" && cargo build -p engine-node --features production --locked --quiet)
# config/anvil.toml with every provider real: a configfs-tsm TEE, a fresh operator key nobody
# publishes, credit at latest, live venues, the checkpoints hardware custody requires, and no plain
# HTTP browser origin. The node refuses it at load, before it reaches any of them.
operator_key="0x$(openssl ecparam -name secp256k1 -genkey -noout 2> /dev/null \
  | openssl ec -pubout -conv_form uncompressed -outform DER 2> /dev/null | tail -c 65 | od -An -v -tx1 | tr -d ' \n')"
[[ "$operator_key" =~ ^0x04[0-9a-f]{128}$ ]] || fail "no fresh operator key: '$operator_key'"
awk -v operator_key="$operator_key" '
  /^\[/ { section = $0 }
  section == "[tee]" && /^provider = / { print "provider = \"configfs_tsm\""; next }
  section == "[tee]" && /^use_published_mock_key/ { next }
  section == "[api]" && /^allowed_origins = / { next }
  section == "[operator]" && /^public_key = / { print "public_key = \"" operator_key "\""; next }
  section == "[custody.evm]" && /^finality_tag = / { print "finality_tag = \"latest\""; next }
  section == "[feeds]" && /^provider = / { print "provider = \"venues\""; next }
  { print }
' "$root/config/anvil.toml" > "$work/production.toml"
sed -n '/^\[feeds.venues\]/,/^\[time\]/p' "$root/config/dev.toml" | sed '$d' >> "$work/production.toml"
cat >> "$work/production.toml" <<'TOML'
[checkpoints]
rpc_url = "http://127.0.0.1:8545"
relayer = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
retry_interval = "5s"
rpc_timeout = "5s"
TOML
grep -q '^allowed_origins' "$work/production.toml" && fail "production config edit missing: no allowed_origins"
for edit in 'provider = "configfs_tsm"' 'finality_tag = "latest"' 'provider = "venues"' '[feeds.venues]'; do
  grep -qF "$edit" "$work/production.toml" || fail "production config edit missing: $edit"
done
if "$engine" run --config "$work/production.toml" > "$work/production.log" 2>&1; then
  fail "the production binary accepted a config without [api.tls.acme]"
fi
grep -q 'a production build refuses the self-signed API certificate' "$work/production.log" \
  || fail "the production binary did not refuse the self-signed certificate: $(cat "$work/production.log")"

echo "PASS: issued, renewed, attested key serves the public certificate"
