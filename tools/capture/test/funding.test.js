// The other half of defect 4: `funding.depth` was the literal 2 on all 5,659
// records, because it was the configured cap echoed back rather than anything
// this launch did. These tests are about what the row now says instead.
import test from 'node:test';
import assert from 'node:assert/strict';
import { Rpc, censusOf } from '../src/rpc.js';
import { unanswered } from '../src/watch.js';

/** A row in the shape `funders()` returns. */
const row = (address, over = {}) => ({
  address,
  funder: null,
  sol: null,
  sig: `sig-${address}`,
  blockTime: 1_700_000_000,
  status: 'none',
  checkedAt: 0,
  ...over,
});

const funded = (address, funder, sol = 1) => row(address, { funder, sol, status: 'ok' });

/**
 * An Rpc whose network is a script: one array of rows per hop, in order.
 * `asked` records what each hop was handed.
 */
class Scripted extends Rpc {
  constructor(hops) {
    super({ url: 'http://endpoint.invalid' });
    this.hops = hops;
    this.asked = [];
  }
  async funders(addresses) {
    this.asked.push([...addresses]);
    return this.hops[this.asked.length - 1] ?? [];
  }
}

// ---------------------------------------------------------------------------
// hopsWalked — what this call actually did
// ---------------------------------------------------------------------------

test('the row says how many hops were walked, not how many were allowed', async () => {
  const rpc = new Scripted([[funded('A', 'F1')], [row('F1')]]);
  const g = await rpc.fundingGraph(['A']);
  assert.equal(g.hopsWalked, 2);
});

test('a launch whose openers resolve to nothing walks one hop, not two', async () => {
  const rpc = new Scripted([[row('A'), row('B')]]);
  const g = await rpc.fundingGraph(['A', 'B']);
  assert.equal(g.hopsWalked, 1, 'there is no second hop when the first found no funder');
  assert.equal(rpc.asked.length, 1, 'and no second request was paid for');
});

test('the configured cap is respected', async () => {
  const rpc = new Scripted([[funded('A', 'F1')], [funded('F1', 'F2')], [funded('F2', 'F3')]]);
  const g = await rpc.fundingGraph(['A'], { depth: 1 });
  assert.equal(g.hopsWalked, 1);
});

test('funding.depth is gone — it never said anything about a launch', async () => {
  const rpc = new Scripted([[funded('A', 'F1')], [row('F1')]]);
  const g = await rpc.fundingGraph(['A']);
  assert.equal('depth' in g, false);
});

// ---------------------------------------------------------------------------
// perHop — what each hop cost and what it bought
// ---------------------------------------------------------------------------

test('perHop says what each hop asked and what came back', async () => {
  const rpc = new Scripted([
    [funded('A', 'F1'), funded('B', 'F1'), row('C')],
    [funded('F1', 'F2')],
  ]);
  const g = await rpc.fundingGraph(['A', 'B', 'C']);
  assert.deepEqual(g.perHop, [
    { hop: 1, asked: 3, resolved: 2 },
    { hop: 2, asked: 1, resolved: 1 },
  ]);
});

test('perHop has exactly one entry per hop walked', async () => {
  const rpc = new Scripted([[funded('A', 'F1')], [row('F1')]]);
  const g = await rpc.fundingGraph(['A']);
  assert.equal(g.perHop.length, g.hopsWalked);
});

test('a funder already seen is not asked about twice', async () => {
  const rpc = new Scripted([[funded('A', 'F1'), funded('B', 'F1')], [row('F1')]]);
  await rpc.fundingGraph(['A', 'B']);
  assert.deepEqual(rpc.asked[1], ['F1']);
});

// ---------------------------------------------------------------------------
// status — why a wallet has no edge
// ---------------------------------------------------------------------------

test('the status census separates what we know from what we could not read', async () => {
  const rpc = new Scripted([[
    funded('A', 'F1'),
    row('B'),
    row('C', { status: 'truncated' }),
    row('D', { status: 'error' }),
  ], [row('F1')]]);
  const g = await rpc.fundingGraph(['A', 'B', 'C', 'D']);
  assert.deepEqual(g.status, { ok: 1, none: 1, truncated: 1, error: 1, notAsked: 0 });
});

test('the census always adds up to the number of wallets asked about', async () => {
  const rpc = new Scripted([[funded('A', 'F1'), row('B', { status: 'error' })], [row('F1')]]);
  const g = await rpc.fundingGraph(['A', 'B']);
  const total = Object.values(g.status).reduce((a, b) => a + b, 0);
  assert.equal(total, g.requested);
});

test('a wallet no answer came back for is notAsked, not unfunded', () => {
  assert.deepEqual(censusOf(['A', 'B'], [funded('A', 'F1')]), {
    ok: 1, none: 0, truncated: 0, error: 0, notAsked: 1,
  });
});

test('a wallet read successfully with no funder behind it counts as none', () => {
  assert.deepEqual(censusOf(['A'], [row('A', { status: 'ok', funder: null })]), {
    ok: 0, none: 1, truncated: 0, error: 0, notAsked: 0,
  });
});

test('an unfamiliar status is counted as an error rather than silently dropped', () => {
  assert.deepEqual(censusOf(['A'], [row('A', { status: 'something-new' })]), {
    ok: 0, none: 0, truncated: 0, error: 1, notAsked: 0,
  });
});

// ---------------------------------------------------------------------------
// The rest of the block
// ---------------------------------------------------------------------------

test('resolved counts only wallets we were asked about, not funders we found', async () => {
  const rpc = new Scripted([[funded('A', 'F1')], [funded('F1', 'F2')]]);
  const g = await rpc.fundingGraph(['A']);
  assert.equal(g.resolved, 1);
  assert.equal(g.unresolved, 0);
  assert.equal(g.transfers.length, 2);
});

test('transfers name both ends and the amount', async () => {
  const rpc = new Scripted([[funded('A', 'F1', 2.5)], [row('F1')]]);
  const g = await rpc.fundingGraph(['A']);
  assert.deepEqual(g.transfers[0], { from: 'F1', to: 'A', sol: 2.5 });
  assert.equal(g.available, true);
});

test('finding nothing is available:false, and it is a claim about the launch', async () => {
  const rpc = new Scripted([[row('A')]]);
  const g = await rpc.fundingGraph(['A']);
  assert.equal(g.available, false);
  assert.equal(g.status.none, 1);
});

test('asking about nobody walks no hops', async () => {
  const g = await new Scripted([]).fundingGraph([]);
  assert.equal(g.hopsWalked, 0);
  assert.deepEqual(g.perHop, []);
  assert.equal(g.status.notAsked, 0);
});

test('an endpoint that was never configured walks no hops', async () => {
  const g = await new Rpc({ url: null }).fundingGraph(['A']);
  assert.equal(g.hopsWalked, 0);
  assert.equal(g.status.notAsked, 1, 'and says so, rather than reporting the wallet as unfunded');
});

test('a shutting-down endpoint walks no hops', async () => {
  const rpc = new Scripted([[funded('A', 'F1')]]);
  rpc.stop();
  const g = await rpc.fundingGraph(['A']);
  assert.equal(g.hopsWalked, 0);
  assert.equal(g.status.notAsked, 1);
});

// ---------------------------------------------------------------------------
// The three ways a launch can have no funding answer
// ---------------------------------------------------------------------------

test('a pending lookup writes the same field names as a finished one', async () => {
  const rpc = new Scripted([[funded('A', 'F1')], [row('F1')]]);
  const real = await rpc.fundingGraph(['A']);
  const placeholder = unanswered(1, { pending: true });
  for (const key of Object.keys(real)) {
    assert.ok(key in placeholder, `a pending block is missing ${key}`);
  }
});

test('a pending lookup says nothing was looked at, not that nothing was there', () => {
  const p = unanswered(3, { pending: true });
  assert.equal(p.pending, true);
  assert.equal(p.available, false);
  assert.equal(p.hopsWalked, 0);
  assert.deepEqual(p.status, { ok: 0, none: 0, truncated: 0, error: 0, notAsked: 3 });
});

test('a failed lookup is marked failed and not confused with a pending one', () => {
  const f = unanswered(2, { pending: false, failed: true });
  assert.equal(f.failed, true);
  assert.equal(f.pending, false);
  assert.equal(f.unresolved, 2);
});
