// The browser verifier's dev scenario harness, run by scripts/dev-wasm-verifier.sh: checks each
// attestation fixture and each engine-wire vector with the WebAssembly package and with the
// `engine` CLI, and fails unless every verdict and message matches and each ends as recorded.
//
// Usage: node dev-wasm-verifier.mjs <package-dir> <engine binary> <fixtures-dir> <vectors.json> <work-dir>

import { spawnSync } from 'node:child_process';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const [packageDir, engine, fixtures, vectorsPath, work] = process.argv.slice(2);
const wasm = await import(pathToFileURL(join(resolve(packageDir), 'engine_verify.js')).href);
await wasm.default({ module_or_path: readFileSync(join(packageDir, 'engine_verify_bg.wasm')) });
mkdirSync(work, { recursive: true });

let compared = 0;
const failures = [];

const text = (dir, name) => readFileSync(join(dir, name), 'utf8');

// A verdict as both sides report it: `{ ok: <the CLI's success line> }` or `{ err: <message> }`.
const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);

// Runs the CLI. Its error is anyhow's report, `Error: a` then each cause; joined with ": " it
// reads as the `{:#}` chain the package throws.
function cli(args) {
  const run = spawnSync(engine, args, { encoding: 'utf8' });
  if (run.status === 0) return { ok: run.stdout.trim().split('\n').pop() };
  const [head, causes = ''] = run.stderr.trim().replace(/^Error: /, '').split('\n\nCaused by:\n');
  const chain = [head, ...causes.split('\n').map((line) => line.trim().replace(/^\d+: /, ''))];
  return { err: chain.filter(Boolean).join(': ') };
}

function wasmVerdict(run) {
  try {
    return { ok: run() };
  } catch (error) {
    return { err: String(error) };
  }
}

// Records a comparison; `expected` is whether the case must verify.
function check(name, native, browser, expected) {
  compared += 1;
  const verified = 'ok' in browser;
  if (!same(native, browser)) failures.push(`${name}: CLI ${JSON.stringify(native)}, wasm ${JSON.stringify(browser)}`);
  else if (verified !== expected) failures.push(`${name}: ${verified ? 'verified' : 'refused'}, expected otherwise`);
  console.log(`${verified ? 'verified' : 'refused '}  ${name}: ${browser.ok ?? browser.err}`);
}

// 1. Attestations.
const { processor, root, cases } = JSON.parse(text(fixtures, 'cases.json'));
let attested;
for (const c of cases) {
  const browser = wasmVerdict(() => {
    const v = wasm.verifyAttestation(
      text(fixtures, c.attestation),
      c.releases.map((release) => text(fixtures, release)),
      text(fixtures, c.collateral),
      Date.now(),
      text(fixtures, root),
    );
    if (v.mock) throw new Error('verifyAttestation reported a mock');
    if (c.attestation === 'attestation.json') attested ??= v.enclaveAddress;
    return `verified: release ${v.releaseName}, enclave ${v.enclaveAddress}, config ${v.configName}`;
  });
  // The CLI takes one release: with several, the first it verifies against is the one to name.
  // A case where the CLI refuses every release of several has no CLI message to compare.
  const runs = c.releases.map((release) =>
    cli(['verify-attestation', '--attestation', join(fixtures, c.attestation), '--release', join(fixtures, release),
      '--processor', processor, '--root', join(fixtures, root), '--collateral', join(fixtures, c.collateral)]));
  const native = runs.length === 1 ? runs[0] : runs.find((run) => 'ok' in run);
  if (native === undefined) {
    failures.push(`attestation: ${c.name}: every release refuses, so the CLI gives no message to compare`);
    continue;
  }
  check(`attestation: ${c.name}`, native, browser, c.verifies);
}

// 2. Signed answers, under the address the attestation verified to.
const vectors = JSON.parse(readFileSync(vectorsPath, 'utf8'));
if (attested !== vectors.signer.toLowerCase()) {
  failures.push(`the attested enclave ${attested} is not the vectors' signer ${vectors.signer}`);
}
const bytes = (s) => new TextEncoder().encode(s);
const answerArgs = (signer, v) =>
  ['--signer', signer, '--chain-id', String(v.chain_id), '--vault', v.vault, '--challenge', v.challenge];
const unattested = '0x' + '11'.repeat(20);

function response(name, v, signer) {
  const dir = join(work, `response-${compared}`);
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, 'request'), v.request_body);
  writeFileSync(join(dir, 'body'), v.body);
  writeFileSync(join(dir, 'headers'), `HTTP/1.1 ${v.status} X\r\nx-engine-signature: ${v.signature}\r\n`
    + `x-engine-time: ${v.time_ms}\r\nx-engine-seq: ${v.head_seq}\r\nx-engine-challenge: ${v.challenge}\r\n\r\n`);
  const native = cli(['verify-response', ...answerArgs(signer, v), '--method', v.method, '--path', v.path_and_query,
    '--request-body', join(dir, 'request'), '--headers', join(dir, 'headers'), '--body', join(dir, 'body')]);
  const browser = wasmVerdict(() => {
    const expected = new wasm.ExpectedSigner(signer, BigInt(v.chain_id), v.vault, v.challenge);
    const r = wasm.verifyResponse(expected, v.method, v.path_and_query, bytes(v.request_body), v.status,
      v.signature, String(v.time_ms), String(v.head_seq), v.challenge, bytes(v.body));
    return `verified: status ${r.status}, signed by ${signer} at ${r.timeMs} ms, log head seq ${r.headSeq}`;
  });
  check(name, native, browser, signer === attested && v.error === null);
}

function connection(name, v, signer) {
  const lines = v.frames.map((frame) => JSON.stringify(frame));
  const path = join(work, `frames-${compared}`);
  writeFileSync(path, lines.join('\n') + '\n');
  const native = cli(['verify-frames', ...answerArgs(signer, v), '--path', v.path_and_query, '--frames', path]);
  const browser = wasmVerdict(() => {
    const frames = new wasm.FrameVerifier(new wasm.ExpectedSigner(signer, BigInt(v.chain_id), v.vault, v.challenge),
      v.path_and_query);
    lines.forEach((line, index) => {
      try {
        if (frames.verifyFrame(line) !== v.frames[index].msg) throw 'the verified message differs from the frame';
      } catch (error) {
        throw `line ${index + 1}: ${error}`;
      }
    });
    return `verified: ${lines.length} frames, n 0 to ${lines.length - 1}, signed by ${signer}`;
  });
  check(name, native, browser, signer === attested && v.error === null);
}

for (const v of vectors.responses) response(`response: ${v.name}`, v, attested);
for (const v of vectors.connections) connection(`connection: ${v.name}`, v, attested);
const valid = (list) => list.find((v) => v.error === null);
response('response: an unattested signer', valid(vectors.responses), unattested);
connection('connection: an unattested signer', valid(vectors.connections), unattested);

// 3. The development entry point reads a mock attestation and says so; nothing else reads through it.
const mock = wasmVerdict(() => wasm.verifyMockAttestation(text(fixtures, 'mock.json')));
if (!mock.ok?.mock || mock.ok.enclaveAddress !== attested || mock.ok.releaseName !== undefined) {
  failures.push(`the mock attestation did not read as a mock of ${attested}: ${mock.err ?? 'wrong fields'}`);
}
if ('ok' in wasmVerdict(() => wasm.verifyMockAttestation(text(fixtures, 'attestation.json')))) {
  failures.push('the development entry point read a hardware quote as a mock');
}
console.log('mock      the development entry point reads the mock attestation and reports mock');

if (failures.length > 0) {
  console.error(failures.map((failure) => `FAIL: ${failure}`).join('\n'));
  process.exit(1);
}
console.log(`PASS: wasm and native verdicts match on ${compared} fixtures`);
