# engine-verify

Verification of an enclave's AMD SEV-SNP attestation against published release manifests.

An attestation is a report, signed by the processor, that identifies the exact code an enclave is
running. `engine-verify` checks one against the release manifests that the caller trusts, and names
the release that admits it. The caller passes in everything the check needs: the attestation that
the server's `/v1/attestation` serves, the release manifests, the vendor's certificates and
revocation list, an optional extra root for a test or private chain, and the time at which to check
dates. The crate performs no input or output, reads no clock and builds no C code. As a result,
`engine verify-attestation`, `engine replay --attestation`, the server's collateral relay and the
[browser verifier](../verify-wasm/README.md) all run this one implementation.

## Usage

`verify_attestation` passes only if all of the following hold:

- the report is of version 2, 3, 5 or 6, as revision 1.59 of AMD's SEV-SNP firmware specification
  defines them, or of the unpublished version 4, read as version 3; and from version 3, which names
  the processor, it comes from CPUID family 19h (Milan, Genoa, Bergamo or Siena), the only family
  whose security-version layout the crate reads;
- the report's certificate chain leads to a vendor root that is compiled into the binary;
- the chip's certificate (the VCEK) matches the report's chip id and reported security version;
- the root's revocation list is current and does not revoke the intermediate certificate;
- the report names the VCEK as its signing key and the VCEK signed it, and the report data binds
  the bundle the server serves;
- a release in the list admits the report by its launch measurement, guest policy, minimum security
  version and configuration hashes.

A release whose `guest_features` lack SNPActive or SecureTSC admits no report. The report does not
carry the guest features, but the launch measurement covers every vCPU's features, so a report that
matches a release's measurement was launched with that release's features.

The result names the release, the enclave address, the configuration and the hash of the TLS key.
It also carries the validity window, the span of time in which every date check passes. When the
list holds one release, a refusal says which check failed. When it holds several, the refusal gives
each release's reason. Errors are messages for a person, which the command-line tools print and the
browser build throws word for word.

```rust
use engine_verify::{ReleaseManifest, RelayedCollateral, verify_attestation};

let attestation = serde_json::from_str(&attestation_json)?; // GET /v1/attestation
let releases = [ReleaseManifest::from_toml(&current)?, ReleaseManifest::from_toml(&previous)?];
let collateral = RelayedCollateral::from_json(collateral_json.as_bytes())?; // GET /v1/attestation/collateral
let verified = verify_attestation(&attestation, &releases, &collateral, None, now_unix_secs)?;
println!("{} runs {} with {}", hex::encode(verified.enclave_address), verified.release_name, verified.config_name);
```

The crate also provides:

- `verify_quote`, which runs the same chain, revocation and signature checks on a report matched to
  no release, and returns its measurement, policy, security version and report data for the caller
  to judge, as an enclave does before it hands its keys to the next release;
- `verify_mock_attestation`, which reads a development server's simulated report and returns only
  the bundle it binds, because a simulated report never verifies;
- `ReleaseManifest`, the release's published TOML, which refuses unknown keys;
- `RelayedCollateral`, the vendor's certificates and revocation list as the server relays them,
  with the bounds and checks the server applies before it serves them;
- `Attestation` and `AttestationBundle`, the attestation's JSON and the bundle its report data
  hashes, which the server builds.

Signatures use the RustCrypto `p384` and `rsa` crates. The vendor's chains use RSASSA-PSS with
SHA-384, and the report and the test chain use ECDSA on P-384 with SHA-384. Any other algorithm is
refused.

## Layout

| Path | Contents |
| --- | --- |
| `src/lib.rs` | `verify_attestation`, `verify_quote`, `verify_mock_attestation` and the matching of releases |
| `src/attestation.rs`, `src/report.rs`, `src/release.rs`, `src/collateral.rs` | The attestation and its bundle, the SEV-SNP report, the release manifest and the relayed collateral |
| `src/chain.rs`, `src/signature.rs`, `src/roots/` | The certificate chain, its signatures and revocation list, and the vendor roots compiled in |
| `src/fixtures.rs` | A stand-in vendor chain, forged reports and a release manifest, behind the `test-fixtures` feature |
| `src/tests/` | Unit tests, with the vendor's real intermediate certificates |

## Configuration

The `test-fixtures` feature exposes the stand-in vendor chain for this crate's tests, the server's
tests and the development scenarios. `examples/verifier_fixtures.rs` writes the attestation fixtures
that the browser verifier's scenario checks.

## Testing

```shell
cargo test -p engine-verify
```

## License

Licensed under the Apache License, Version 2.0, whose text is in [LICENSE](../../LICENSE).
