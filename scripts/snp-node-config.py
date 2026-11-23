"""Writes an SEV-SNP guest's node.toml from config/anvil.toml for scripts/snp-local-chain.sh: real
attestation with sealed keys and delivered secrets, the Siena CPU model under the given product name
as the one a peer guest's quote may come from, paths under /data, listeners on every address,
the chain reached over TLS at <rpc-host> on four ports (three custody providers and the checkpoint
endpoint), deposits credited at `latest`, and the given vault, token, MONKEY minter, checkpoint relayer
and operator key. crates/node/tests/snp_guest_config.rs loads what it writes.

Usage: python3 -I scripts/snp-node-config.py <anvil.toml> <out> <rpc-host> <vault> <usdc> <minter>
       <relayer> <operator-public-key> <processor> <product-name> <custody-port> <custody-port>
       <custody-port> <checkpoint-port>
"""

import sys
from pathlib import Path

source, target, host, vault, usdc, minter, relayer, public_key, processor, product_name, *ports = sys.argv[1:]
assert len(ports) == 4, "four ports: three custody providers and the checkpoint endpoint"
text = Path(source).read_text()
old_key = text.split('public_key = "', 1)[1].split('"', 1)[0]
for literal, replacement in {
    '[node]\nlisten = "127.0.0.1:8080"': '[node]\nlisten = "0.0.0.0:8080"',
    '"data/anvil/': '"/data/',
    'provider = "mock"\nuse_published_mock_key = true # Public development identity; forbidden with real attestation.':
        'provider = "configfs_tsm"\nprocessor = "%s"\n\n'
        '# The CPU model a peer guest\'s quote may come from: Siena, as its VCEK names it, with no TCB\n'
        '# floor on this local chain. release.toml lists the same model.\n'
        '[[tee.cpu_models]]\nproduct_name = "%s"\nfamily = 0x19\nmodel = 0xA0\nstepping = 0x02\n'
        'minimum_tcb = { boot_loader = 0, tee = 0, snp = 0, microcode = 0 }\n\n'
        '[tee.sealing]\ndir = "/data/keys"\n\n'
        '# Where the operator delivers secrets, such as the Critical alert route.\n'
        '[tee.sealing.secrets]\nlisten = "0.0.0.0:9472"' % (processor, product_name),
    old_key: public_key,
    'vault = "0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512"': 'vault = "%s"' % vault,
    'usdc = "0x5FbDB2315678afecb367f032d93F642f64180aa3"': 'usdc = "%s"' % usdc,
    'minter = "0x9fE46736679d2D9a65F0992F2272dE9f3c7fa6e0"': 'minter = "%s"' % minter,
    '"http://127.0.0.1:8545"': '"https://%s:%s"' % (host, ports[0]),
    '"http://127.0.0.1:8546"': '"https://%s:%s"' % (host, ports[1]),
    '"http://127.0.0.1:8547"': '"https://%s:%s"' % (host, ports[2]),
    'url_env = "ENGINE_ALERTS_CRITICAL_URL"': 'url_secret = "alerts_critical_url"',
    # Real attestation credits a deposit at `latest`, after the confirmations.
    'finality_tag = "safe"': 'finality_tag = "latest"',
    '[metrics]\nlisten = "127.0.0.1:9464"': '[metrics]\nlisten = "0.0.0.0:9464"',
}.items():
    assert text.count(literal) >= 1, literal
    text = text.replace(literal, replacement)
text += """
# The vault names the release that opens the keys; Anvil signs the relayer's transactions.
[checkpoints]
rpc_url = "https://%s:%s"
relayer = "%s"
retry_interval = "5s"
rpc_timeout = "5s"
""" % (host, ports[3], relayer)
Path(target).write_text(text)
