// End-to-end regtest run of the LDK Node offline-receive runtime against Beignet reference
// daemons: S settles, W witnesses, X pays through W. The receiver R is the
// `ffor_regtest_receiver` example driven over its stdout line protocol.
//
// Flow: create and fund S, W, X; open W -> S and X -> W (announced); start R and let S open an
// anchor channel to it; R prepares one offline receive and prints the invoice; R is killed;
// R restarts once before payment and must report the same invoice (negative case); R is
// killed again; X pays through W while R is down; W holds one record; R restarts, re-releases
// the invoice, the runtime closes the epoch at the deadline margin, credits exactly one
// PaymentReceived and reports Settled; a final restart confirms no second event and a
// Succeeded payment row.
//
// Everything this script starts (manager, daemons, receiver) is stopped on exit.
import { spawn } from 'node:child_process';
import { createWriteStream, mkdirSync, rmSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const LDK_NODE = path.resolve(HERE, '../..');
const FFOR_ROOT = path.resolve(LDK_NODE, '..');
const UMBREL = process.env.BEIGNET_UMBREL_DIR || path.join(FFOR_ROOT, 'beignet-umbrel');
const BEIGNET_BIN = process.env.BEIGNET_BIN || path.join(FFOR_ROOT, 'beignet/dist/cli/cli.js');
const STATE = process.env.FFOR_E2E_DIR || '/private/tmp/ffor-e2e';
const LOG_DIR = process.env.FFOR_E2E_LOG_DIR || '/private/tmp';
const RECEIVER_BIN = process.env.RECEIVER_BIN || path.join(LDK_NODE, 'target/debug/examples/ffor_regtest_receiver');
const MANAGER_PORT = Number(process.env.MANAGER_PORT || 3900);
const ELECTRUM_HOST = process.env.ELECTRUM_HOST || '127.0.0.1';
const ELECTRUM_PORT = Number(process.env.ELECTRUM_PORT || 60001);
const R_PORT = Number(process.env.RECEIVER_PORT || 9739);
const REQUEST_ID = 'e2e-1';
const VOUCHER_SATS = 50000;
const VOUCHER_MSAT = VOUCHER_SATS * 1000;
// Development defaults of OfflineReceiveConfig: the runtime closes at tip + margin >= deadline.
const SETTLEMENT_DEADLINE_BLOCKS = 144;
const R_FUNDING_SATS = 1_000_000;
const DEADLINE_SAFETY_MARGIN_BLOCKS = 6;

// lib.mjs reads its environment at import time: force the docker chain path and no CLN.
delete process.env.REGTEST_API;
process.env.CLN_CONTAINER = '';
process.env.MANAGER_URL = `http://127.0.0.1:${MANAGER_PORT}`;
const lib = await import(pathToFileURL(path.join(UMBREL, 'scripts/lfbw-regtest/lib.mjs')).href);
const { api, w, btc, fund, healthy, waitFor, chainTip, mine, check, log, sleep, listenPortOf, PRIMARY_LOCAL_HOST } = lib;

const failures = [];
const origCheck = check;
const verdict = (name, ok, extra = '') => {
	if (!ok) failures.push(name);
	origCheck(name, ok, extra);
};

const channelWith = async (id, peerNode) =>
	(await w(id, '/channels')).find((c) => c.peerPubkey === peerNode && !/CLOSED|CLOSING/.test(c.state)) || null;

async function openSiblingChannel(from, to, sats) {
	const toRec = await api(`/wallets/${to}`);
	const fromRec = await api(`/wallets/${from}`);
	const open = await w(from, '/channel/connect-and-open', {
		method: 'POST',
		body: { pubkey: toRec.nodeId, host: PRIMARY_LOCAL_HOST, port: await listenPortOf(to), amountSats: sats }
	});
	log(`  ${fromRec.name} -> ${toRec.name} open`, open.state || JSON.stringify(open).slice(0, 80));
	await sleep(4000);
	await mine(6);
	await waitFor(`${fromRec.name} -> ${toRec.name} channel NORMAL on both sides`, async () => {
		const a = await channelWith(from, toRec.nodeId);
		const b = await channelWith(to, fromRec.nodeId);
		return a && b && a.state === 'NORMAL' && b.state === 'NORMAL' && (a.htlcUsable ?? true) && (b.htlcUsable ?? true) ? a : null;
	}, { timeoutMs: 180000, everyMs: 3000 });
	return channelWith(from, toRec.nodeId);
}

// ---- process helpers ----

function spawnLogged(name, cmd, args, opts) {
	const out = createWriteStream(path.join(LOG_DIR, `ffor-e2e-${name}.log`), { flags: 'a' });
	const child = spawn(cmd, args, { ...opts, stdio: ['ignore', 'pipe', 'pipe'] });
	child.stdout.pipe(out);
	child.stderr.pipe(out);
	return child;
}

async function startManager() {
	mkdirSync(path.join(STATE, 'manager'), { recursive: true });
	const env = {
		...process.env,
		PORT: String(MANAGER_PORT),
		DATA_DIR: path.join(STATE, 'manager'),
		BEIGNET_BIN,
		DEFAULT_NETWORK: 'regtest',
		DEFAULT_ELECTRUM_HOST: ELECTRUM_HOST,
		DEFAULT_ELECTRUM_PORT: String(ELECTRUM_PORT),
		DEFAULT_ELECTRUM_TLS: 'false',
		CHILD_PORT_BASE: '3901',
		CHILD_PORT_MAX: '3950',
		BEIGNET_TRUST_ALL: '1'
	};
	const child = spawnLogged('manager', 'node', ['server/index.js'], { cwd: path.join(UMBREL, 'manager'), env });
	await waitFor('manager up', () => api('/wallets').then(() => true), { timeoutMs: 30000 });
	return child;
}

/** The receiver example as a child process with a line reader over stdout. */
class Receiver {
	constructor(args) {
		this.args = args;
		this.proc = null;
		this.lines = [];
		this.log = createWriteStream(path.join(LOG_DIR, 'ffor-e2e-receiver.log'), { flags: 'a' });
	}
	start(command) {
		this.lines = [];
		this.exited = null;
		const argv = [...this.args, command, REQUEST_ID, String(VOUCHER_MSAT)];
		this.log.write(`\n===== ${new Date().toISOString()} ${command}\n`);
		this.proc = spawn(RECEIVER_BIN, argv, { stdio: ['pipe', 'pipe', 'pipe'] });
		let buf = '';
		this.proc.stdout.on('data', (d) => {
			buf += d.toString();
			let i;
			while ((i = buf.indexOf('\n')) >= 0) {
				const line = buf.slice(0, i);
				buf = buf.slice(i + 1);
				this.lines.push(line);
				this.log.write(line + '\n');
				log(`  [R ${command}] ${line}`);
			}
		});
		this.proc.stderr.on('data', (d) => this.log.write(d));
		this.exit = new Promise((resolve) => this.proc.on('exit', (code, sig) => { this.exited = { code, sig }; resolve({ code, sig }); }));
		return this;
	}
	find(re) { return this.lines.find((l) => re.test(l)) || null; }
	count(re) { return this.lines.filter((l) => re.test(l)).length; }
	waitLine(desc, re, timeoutMs = 120000) {
		return waitFor(desc, async () => {
			if (this.exited) throw new Error(`receiver exited ${JSON.stringify(this.exited)} while waiting for ${desc}`);
			return this.find(re);
		}, { timeoutMs, everyMs: 500 }).catch((e) => { if (/receiver exited/.test(e.message)) throw e; throw new Error(`timeout waiting for ${desc}`); });
	}
	kill() {
		if (this.proc && !this.exited) this.proc.kill('SIGKILL');
		return this.exit;
	}
	async closeGracefully(timeoutMs = 60000) {
		if (!this.proc || this.exited) return this.exited;
		this.proc.stdin.end();
		const result = await Promise.race([this.exit, sleep(timeoutMs).then(() => null)]);
		if (!result) { this.proc.kill('SIGKILL'); await this.exit; }
		return this.exited;
	}
}

// ---- the run ----

const mk = (name, extra = {}) => api('/wallets', { method: 'POST', body: { name, network: 'regtest', ...extra } }).then((r) => r.record);

rmSync(STATE, { recursive: true, force: true });
mkdirSync(STATE, { recursive: true });
const receiverDir = path.join(STATE, 'receiver');
mkdirSync(receiverDir, { recursive: true });

let manager = null;
let receiver = null;
const wallets = [];
try {
	manager = await startManager();
	const S = await mk('Settler', { ffor: { settle: { enabled: true } } });
	const W = await mk('Witness', { ffor: { witness: { enabled: true } } });
	const X = await mk('Payer');
	wallets.push(S.id, W.id, X.id);
	await Promise.all([healthy(S.id), healthy(W.id), healthy(X.id)]);
	const Srec = await api(`/wallets/${S.id}`);
	const Wrec = await api(`/wallets/${W.id}`);
	verdict('S carries the settle role and W the witness role', Srec.ffor?.settle?.enabled === true && Wrec.ffor?.witness?.enabled === true, JSON.stringify({ s: Srec.ffor, w: Wrec.ffor }));
	const wStatus = await w(W.id, '/ffor/witness/status');
	verdict('W runs the witness service', wStatus.enabled === true, JSON.stringify(wStatus));

	await fund(S.id, 3_000_000);
	await fund(W.id, 3_000_000);
	await fund(X.id, 3_000_000);
	await waitFor('S, W and X funded', async () => (await w(S.id, '/balance')).onchain >= 3_000_000 && (await w(W.id, '/balance')).onchain >= 3_000_000 && (await w(X.id, '/balance')).onchain >= 3_000_000);
	await openSiblingChannel(W.id, S.id, 1_000_000);
	await openSiblingChannel(X.id, W.id, 500_000);
	await waitFor('X\'s graph carries the W -> S channel', async () => { const h = await w(X.id, '/health'); return h.graphChannels >= 2 ? h : null; }, { timeoutMs: 180000, everyMs: 3000 });

	// R: the LDK Node receiver.
	const sPort = await listenPortOf(S.id);
	const wPort = await listenPortOf(W.id);
	receiver = new Receiver([receiverDir, String(R_PORT), `tcp://${ELECTRUM_HOST}:${ELECTRUM_PORT}`, Srec.nodeId, `127.0.0.1:${sPort}`, Wrec.nodeId, `127.0.0.1:${wPort}`]);
	receiver.start('serve');
	const nodeIdLine = await receiver.waitLine('R node id', /^NODEID /, 60000);
	const rNodeId = nodeIdLine.split(' ')[1];
	await receiver.waitLine('R listening', /^LISTENING /, 60000);
	await receiver.waitLine('R connected to S', /^CONNECTED settlement /, 60000);
	await receiver.waitLine('R connected to W', /^CONNECTED witness /, 60000);
	verdict('R connected to S and W', true);

	// R funds and opens the anchor channel to S, pushing most of it to S. Beignet's v1 open pins
	// the anchor commitment feerate to the 253 sat/kw floor, which LDK refuses when its
	// 1008-block estimate on this regtest is higher ("Peer's feerate much too low"), and LDK
	// does not advertise option_dual_fund, so S cannot name a feerate through a v2 open.
	const addressLine = await receiver.waitLine('R funding address', /^ADDRESS /, 60000);
	const rAddress = addressLine.split(' ')[1];
	const fundingTxid = await btc(`sendtoaddress ${rAddress} ${(R_FUNDING_SATS / 1e8).toFixed(8)}`);
	await mine(1);
	log(`  funded R with ${R_FUNDING_SATS} sats (${fundingTxid.slice(0, 12)})`);
	await receiver.waitLine('R saw its funding', /^FUNDED /, 180000);
	const openLine = await receiver.waitLine('R opened the channel to S', /^OPENED /, 60000);
	verdict('R opened a channel to S', !!openLine, openLine);
	await sleep(4000);
	await mine(6);
	const sChannel = await waitFor('R -> S channel NORMAL and usable on S', async () => { const c = await channelWith(S.id, rNodeId); return c && c.state === 'NORMAL' && (c.htlcUsable ?? true) ? c : null; }, { timeoutMs: 180000, everyMs: 3000 });
	verdict('R -> S is an anchor channel in NORMAL state', sChannel.isAnchor === true && sChannel.state === 'NORMAL', JSON.stringify({ isAnchor: sChannel.isAnchor, state: sChannel.state, isPrivate: sChannel.isPrivate }));
	await receiver.waitLine('R sees the channel ready', /^CHANNEL_READY /, 180000);
	verdict('R reports the channel with S ready', true, receiver.find(/^CHANNEL_READY /));
	// Beignet sends a channel_update for an unannounced channel only when its policy changes;
	// LDK's native invoice binding needs S's forwarding terms (cltv delta) for the route hint,
	// so push S's (unchanged) policy for this channel before R prepares.
	await receiver.waitLine('R awaits the prepare signal', /^AWAITING_PREPARE$/, 60000);
	const policy = await w(S.id, '/channel/update-policy', { method: 'POST', body: { channelId: sChannel.channelId, feeBaseMsat: 0, feeProportionalMillionths: 0 } });
	verdict('S pushed its channel_update for the R channel', true, JSON.stringify(policy).slice(0, 120));
	await sleep(3000);
	const tipAtPrepare = await chainTip();
	receiver.proc.stdin.write('prepare\n');
	await receiver.waitLine('R prepared the request', /^PREPARED /, 30000);

	// Invoice.
	const invoiceLine = await receiver.waitLine('R printed the offline invoice', /^INVOICE /, 240000);
	const bolt11 = invoiceLine.slice('INVOICE '.length).trim();
	verdict('R reached Ready with a BOLT 11 invoice', /^lnbcrt/.test(bolt11), bolt11.slice(0, 40) + '...');
	const settlementsA = await w(S.id, '/ffor/settlements');
	const epoch = settlementsA.find((e) => e.state === 'ACTIVE');
	verdict('S lists the epoch ACTIVE', !!epoch, JSON.stringify(settlementsA.map((e) => e.state)));
	const mbA = await w(W.id, '/ffor/witness/status');
	verdict('W holds one provisioned mailbox with no record yet', mbA.mailboxes.length === 1 && mbA.mailboxes[0].state === 'PROVISIONED' && (mbA.mailboxes[0].records || 0) === 0, JSON.stringify(mbA.mailboxes));

	// R goes down hard.
	await receiver.kill();
	verdict('R killed (SIGKILL)', receiver.exited?.sig === 'SIGKILL', JSON.stringify(receiver.exited));

	// Negative case: a restart before payment re-releases the same invoice, never a new one.
	receiver.start('status');
	const reissued = await receiver.waitLine('R restarted and reported Ready again', /^INVOICE /, 240000);
	const bolt11Again = reissued.slice('INVOICE '.length).trim();
	verdict('restart before payment reports the identical invoice', bolt11Again === bolt11, bolt11Again === bolt11 ? 'same bolt11' : `different: ${bolt11Again.slice(0, 40)}`);
	verdict('restart before payment credited nothing', receiver.count(/^EVENT PaymentReceived/) === 0);
	await receiver.kill();

	// X pays through W while R is down.
	const paid = await w(X.id, '/invoice/pay-safe', { method: 'POST', body: { bolt11 } });
	verdict('X paid the invoice through W while R was down', paid.status === 'COMPLETED', `${paid.status} ${paid.failureDescription || ''}`);
	const recorded = await waitFor('W kept a receipt', async () => { const s = await w(W.id, '/ffor/witness/status'); return s.mailboxes[0] && s.mailboxes[0].records >= 1 ? s : null; }, { timeoutMs: 30000 });
	verdict('W holds exactly one record', recorded.mailboxes[0].records === 1, JSON.stringify(recorded.mailboxes[0]));
	const settledOnS = await waitFor('S marks slot 1 settled', async () => {
		const e = (await w(S.id, '/ffor/settlements')).find((x) => x.epochId === epoch.epochId);
		return e && e.slots && e.slots[0] && e.slots[0].state === 'settled' ? e : null;
	}, { timeoutMs: 30000 });
	verdict('S settled slot 1', settledOnS.slots[0].state === 'settled', JSON.stringify(settledOnS.slots.map((s) => s.state)));

	// R returns: re-release, then the deadline margin closes the epoch and credits.
	receiver.start('status');
	await receiver.waitLine('R reconnected to S after restart', /^CONNECTED settlement /, 60000);
	await receiver.waitLine('R re-released the invoice after payment', /^INVOICE /, 240000);
	verdict('the re-released invoice after payment is the same bolt11', receiver.find(/^INVOICE /).slice('INVOICE '.length).trim() === bolt11);
	// The runtime closes when tip + margin >= deadline; the receiver's chain view may trail the
	// chain by one sync interval, so reach the margin exactly and then add single blocks.
	const deadline = tipAtPrepare + SETTLEMENT_DEADLINE_BLOCKS;
	const target = deadline - DEADLINE_SAFETY_MARGIN_BLOCKS;
	const now = await chainTip();
	log(`  mining ${Math.max(0, target - now)} blocks to reach the deadline margin (tip ${now}, deadline ${deadline})`);
	if (target > now) await mine(target - now);
	for (let extra = 0; ; extra++) {
		const left = await receiver.waitLine('R requested the close (status leaves Ready)', /^STATUS (Expired|Settled|Failed)/, 45000).catch(() => null);
		if (left) break;
		if (extra >= 3) throw new Error('the runtime did not leave Ready at the deadline margin');
		log(`  one more block (tip ${await chainTip()})`);
		await mine(1);
	}
	const settledLine = await receiver.waitLine('R settled the request', /^STATUS Settled/, 300000);
	verdict('R reports Settled { Fulfilled }', /Fulfilled/.test(settledLine), settledLine);
	await receiver.waitLine('R emitted PaymentReceived', /^EVENT PaymentReceived/, 60000);
	await sleep(8000);
	verdict('exactly one PaymentReceived event', receiver.count(/^EVENT PaymentReceived/) === 1, String(receiver.count(/^EVENT PaymentReceived/)));
	const evt = receiver.find(/^EVENT PaymentReceived/);
	verdict('the credited amount is the voucher amount', new RegExp(`amount_msat=${VOUCHER_MSAT}$`).test(evt), evt);
	const sClosed = (await w(S.id, '/ffor/settlements')).find((x) => x.epochId === epoch.epochId);
	verdict('S reads the epoch CLOSED with slot 1 settled', !!sClosed && sClosed.state === 'CLOSED' && sClosed.slots[0].state === 'settled', sClosed ? JSON.stringify({ state: sClosed.state, slots: sClosed.slots.map((s) => s.state) }) : 'gone');
	const stopped = await receiver.closeGracefully();
	verdict('R stopped cleanly on stdin close', stopped?.code === 0 && !!receiver.find(/^STOPPED$/), JSON.stringify(stopped));
	const rowAfter = receiver.lines.filter((l) => /^PAYMENT /.test(l)).pop();
	verdict('the payment row reads Succeeded after settlement', !!rowAfter && /status=Succeeded/.test(rowAfter) && /direction=Inbound/.test(rowAfter), rowAfter || 'no PAYMENT line');

	// One more restart: no second event, the row stays Succeeded.
	receiver.start('status');
	await receiver.waitLine('R restarted after settlement', /^STATUS /, 120000);
	await receiver.waitLine('R reports Settled again', /^STATUS Settled/, 120000);
	await sleep(15000);
	verdict('no second PaymentReceived after the final restart', receiver.count(/^EVENT PaymentReceived/) === 0, String(receiver.count(/^EVENT PaymentReceived/)));
	const rowFinal = receiver.lines.find((l) => /^PAYMENT /.test(l));
	verdict('the payment row is Succeeded at startup of the final restart', !!rowFinal && /status=Succeeded/.test(rowFinal), rowFinal || 'no PAYMENT line');
	await receiver.closeGracefully();
} catch (e) {
	verdict(`run aborted: ${e.message}`, false, e.stack ? e.stack.split('\n').slice(0, 3).join(' | ') : '');
} finally {
	if (receiver) await receiver.kill().catch(() => {});
	for (const id of wallets) {
		await api(`/wallets/${id}/stop`, { method: 'POST' }).then(() => log(`  stopped wallet ${id}`)).catch((e) => log(`  stop ${id}: ${e.message}`));
	}
	if (manager) {
		manager.kill('SIGTERM');
		await Promise.race([new Promise((r) => manager.on('exit', r)), sleep(15000)]);
		if (manager.exitCode === null) manager.kill('SIGKILL');
	}
}
log(failures.length ? `RESULT FAIL (${failures.length})` : 'RESULT PASS');
process.exit(failures.length ? 1 : 0);
