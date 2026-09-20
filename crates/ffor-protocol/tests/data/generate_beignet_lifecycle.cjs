// Reproduce lifecycle interoperability fixtures using the pinned Beignet implementation.
// Usage: node -r /path/to/beignet/node_modules/ts-node/register \
//   generate_beignet_lifecycle.cjs /path/to/beignet /path/to/output.json
const fs = require('node:fs');
const path = require('node:path');
const { execFileSync } = require('node:child_process');

const reference = path.resolve(process.argv[2]);
const expectedRevision = '8aee31d18e596fe49a0d195b325a6e757d7a009b';
const revision = execFileSync('git', ['rev-parse', 'HEAD'], { cwd: reference, encoding: 'utf8' }).trim();
if (revision !== expectedRevision) throw new Error('Beignet revision differs from the fixture baseline');
const codecs = require(path.join(reference, 'src/lightning/ffor/messages.ts'));
const { getPublicKey } = require(path.join(reference, 'src/lightning/crypto/ecdh.ts'));

// Deterministic test-only signing material. These fixtures contain no wallet keys or funds.
const secret = Buffer.alloc(32, 42);
const publicKey = getPublicKey(secret);
const header = { channelId: Buffer.alloc(32, 1), epochId: Buffer.alloc(32, 2) };
const activation = { ...header, activationHash: Buffer.alloc(32, 9) };
const emptyClose = codecs.encodeFforCloseAckUnsigned({
    ...activation, numSlots: 2, settled: Buffer.from([0]), preimages: []
});
const cases = [
    ['abort', 55049, codecs.encodeFforAbortUnsigned({
        ...header, transcriptHash: Buffer.alloc(32, 8), reason: 6,
        data: Buffer.from('disconnected during setup', 'utf8')
    })],
    ['close', 55051, codecs.encodeFforCloseUnsigned(activation)],
    ['close_ack_paid', 55053, codecs.encodeFforCloseAckUnsigned({
        ...activation, numSlots: 2, settled: Buffer.from([1]),
        preimages: [{ k: 1, preimage: Buffer.alloc(32, 5) }]
    })],
    ['close_ack_unpaid_absent_tlv', 55053, emptyClose],
    ['close_ack_unpaid_empty_tlv', 55053, Buffer.concat([emptyClose, Buffer.from([1, 0])])]
];
const fixtures = cases.map(([name, type, unsigned]) => {
    const body = codecs.signFforMessage(type, unsigned, secret);
    if (!codecs.verifyFforMessage(type, body, publicKey)) throw new Error('Fixture authentication failed');
    return { name, message_type: type, signer: publicKey.toString('hex'), wire: codecs.fforWireBytes(type, body).toString('hex') };
});
const reestablish = Array.from({ length: 7 }, (_, state) => {
    // Beignet retains hAct while ACTIVATING, even though section 11.1 says zero before ACTIVE.
    // Preserve that input as an interoperability case, without treating it as activation proof.
    const activationHash = Buffer.alloc(32, state >= 2 && state <= 5 ? 9 : 0);
    const tlv = codecs.encodeFforReestablishTlv({ epochId: header.epochId, state, lastSeq: 0, activationHash });
    const decoded = codecs.decodeFforReestablishTlv(tlv.value);
    if (decoded.state !== state || !decoded.activationHash.equals(activationHash)) throw new Error('Reconnect fixture mismatch');
    return { state, type: Number(tlv.type), value: tlv.value.toString('hex') };
});
fs.writeFileSync(process.argv[3], JSON.stringify({ source_revision: revision, fixtures, reestablish }, null, 2) + '\n');
