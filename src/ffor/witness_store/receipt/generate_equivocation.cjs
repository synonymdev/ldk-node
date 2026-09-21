// Public deterministic D.1 alternate evidence, never production key material.
// TS_NODE_PROJECT=/path/beignet/tsconfig.json node -r /path/beignet/node_modules/ts-node/register \
//   generate_equivocation.cjs /path/beignet ../record/key_use/fixtures.txt equivocation.txt
const fs = require('node:fs'), path = require('node:path'), crypto = require('node:crypto');
const { execFileSync } = require('node:child_process');
const reference = path.resolve(process.argv[2]);
if (execFileSync('git', ['rev-parse', 'HEAD'], {cwd: reference, encoding: 'utf8'}).trim() !== '8aee31d18e596fe49a0d195b325a6e757d7a009b') throw Error('Wrong reference');
const codecs = require(path.join(reference, 'src/lightning/ffor/witness-messages.ts'));
const encryption = require(path.join(reference, 'src/lightning/ffor/witness-crypto.ts'));
const curve = require(path.join(reference, 'src/lightning/crypto/ecdh.ts'));
const fixture = Object.fromEntries(fs.readFileSync(process.argv[3], 'utf8').trim().split('\n\n')[0].split('\n').map(line => line.split('=')));
const record = codecs.decodeRecord(Buffer.from(fixture.record, 'hex'));
record.header.recordId = Buffer.alloc(32, 99);
const savedRandom = crypto.randomBytes;
try {
 crypto.randomBytes = length => { if (length !== 32) throw Error('Unexpected entropy'); return Buffer.alloc(32, 46); };
 record.ciphertext = encryption.sealRecordBody(record.header.encPubkey, codecs.recordAad(record.header), Buffer.from(fixture.body, 'hex'));
} finally { crypto.randomBytes = savedRandom; }
record.header.ciphertextHash = crypto.createHash('sha256').update(record.ciphertext).digest();
record.witnessSig = curve.sign(codecs.recordDigest(codecs.encodeRecordHeader(record.header)), Buffer.alloc(32, 43));
if (!codecs.verifyRecordSignature(record)) throw Error('Signature');
if (!encryption.openRecordBody(Buffer.alloc(32, 44), codecs.recordAad(record.header), record.ciphertext).equals(Buffer.from(fixture.body, 'hex'))) throw Error('AEAD');
fs.writeFileSync(process.argv[4], codecs.encodeRecord(record).toString('hex')+'\n');
