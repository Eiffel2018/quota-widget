'use strict';
const {test} = require('node:test');
const assert = require('node:assert/strict');
const {UsageHistory, MAX_SAMPLES, MAX_EVENTS} = require('../src/core/history.cjs');

const bucket = (remaining = 80, resetsAt = 10000, extra = {}) => [{id: 'sol', name: 'Sol',
  windows: [{kind: 'primary', remaining, durationMins: 10080, resetsAt}], ...extra}];

test('observations preserve reported precision and allow only quota data', () => {
  const h = new UsageHistory();
  h.observe(bucket(78.75, 10000, {accountId: 'private', email: 'private@example.com'}), 0);
  const state = h.snapshot();
  assert.equal(state.samples[0].remaining, 78.75);
  assert.equal(state.samples[0].timestamp, 0);
  assert.equal(state.samples[0].elapsedMs, null);
  assert.equal(state.events.length, 0);
  assert.ok(!JSON.stringify(h).includes('private'));
  state.samples[0].remaining = 0;
  assert.equal(h.snapshot().samples[0].remaining, 78.75);
});

test('remaining increase and reset timestamp changes create one unknown-source event', () => {
  const h = new UsageHistory();
  h.observe(bucket(30), 0);
  h.observe(bucket(20), 60000);
  h.observe(bucket(100, 20000), 120000);
  const [event] = h.snapshot().events;
  assert.equal(h.snapshot().events.length, 1);
  assert.equal(event.source, 'unknown');
  assert.deepEqual(event.reasons, ['remaining-increase', 'reset-time-changed']);
  assert.equal(event.before.remaining, 20);
  assert.equal(event.after.remaining, 100);
  assert.equal(event.elapsedMs, 60000);
  assert.equal(event.gap, false);
  assert.equal(event.timestamp, 120000);
  assert.ok(event.id);
});

test('timestamp-only changes log an observation, not an asserted reset cause', () => {
  const h = new UsageHistory();
  h.observe(bucket(30), 0);
  h.observe(bucket(25, 20000), 60000);
  assert.deepEqual(h.snapshot().events[0].reasons, ['reset-time-changed']);
  assert.equal(h.snapshot().events[0].source, 'unknown');
});

test('unknown values and malformed inputs never become zero or fake resets', () => {
  const h = new UsageHistory();
  h.observe(bucket(null, null), 0);
  h.observe(bucket(100), 60000);
  assert.equal(h.snapshot().events.length, 0);
  for (const value of [undefined, null, NaN, Infinity, -1, 101, '20']) {
    const input = bucket();
    input[0].windows[0].remaining = value;
    h.observe(input, 120000);
    assert.equal(h.snapshot().samples.at(-1).remaining, null);
  }
  const before = h.snapshot();
  h.observe(null, 1);
  h.observe(bucket(), NaN);
  assert.deepEqual(h.snapshot(), before);
  h.observe([null, {}, {id: 'x', windows: [null, {}]}], 200000);
});

test('credits are numeric, distinct from percent, and top-ups are observed', () => {
  const h = new UsageHistory();
  const credits = balance => [{id: 'credits-only', windows: [], credits: {balance, hasCredits: true}}];
  h.observe(credits('62416'), 0);
  h.observe(credits('62000.5'), 60000);
  h.observe(credits('63000.5'), 120000);
  const {samples, events} = h.snapshot();
  assert.equal(samples[0].window, 'credits');
  assert.equal(samples[0].balance, 62416);
  assert.equal(samples[0].remaining, null);
  assert.deepEqual(events[0].reasons, ['balance-increase']);
  assert.equal(events[0].before.balance, 62000.5);
  assert.equal(events[0].after.balance, 63000.5);
  h.observe(credits('99999999999999999999'), 180000);
  assert.equal(h.snapshot().samples.at(-1).balance, null);
  h.observe([{id: 'credits-only', credits: {balance: '90000', unlimited: true}}], 240000);
  assert.equal(h.snapshot().samples.at(-1).balance, null);
});

test('history is bounded and restores only bounded, allowlisted records', () => {
  const h = new UsageHistory();
  for (let n = 0; n < MAX_SAMPLES + 5; n++) h.observe(bucket(n % 2 ? 100 : 10), n * 60000);
  const state = h.snapshot();
  assert.equal(state.samples.length, MAX_SAMPLES);
  assert.equal(state.events.length, MAX_EVENTS);
  assert.equal(state.samples[0].timestamp, 5 * 60000);
  const restored = new UsageHistory(JSON.parse(JSON.stringify(h)));
  assert.deepEqual(restored.snapshot(), state);
  restored.observe(bucket(100, 30000), (MAX_SAMPLES + 5) * 60000);
  assert.deepEqual(restored.snapshot().events, state.events);
  assert.notEqual(restored.snapshot().samples.at(-1).sessionId, state.samples.at(-1).sessionId);
});

test('malformed restored state is safe and extra fields are dropped', () => {
  for (const input of [null, undefined, false, '', [], {samples: {}, events: null}]) {
    assert.deepEqual(new UsageHistory(input).snapshot(), {samples: [], events: []});
  }
  const good = new UsageHistory();
  good.observe(bucket(20), 0);
  good.observe(bucket(90), 60000);
  const state = good.snapshot();
  state.samples[0].accountId = 'private';
  state.samples[0].remaining = Infinity;
  state.events[0].note = 'x\u0000\ny';
  state.events[0].source = 'guessed';
  state.events[0].before.password = 'private';
  state.samples.push(null, {timestamp: NaN}, {timestamp: 1e30}, {});
  state.events.push(null, {}, {timestamp: 1});
  const restored = new UsageHistory(state).snapshot();
  assert.equal(restored.samples.length, 2);
  assert.equal(restored.events.length, 1);
  assert.equal(restored.samples[0].remaining, null);
  assert.equal(restored.events[0].source, 'unknown');
  assert.equal(restored.events[0].note, 'x  y');
  assert.ok(!JSON.stringify(restored).includes('private'));
});

test('manual labels are validated and notes sanitized and capped', () => {
  const h = new UsageHistory();
  h.observe(bucket(20), 0);
  h.observe(bucket(90), 60000);
  const id = h.snapshot().events[0].id;
  const tagged = h.tag(id, {source: 'coupon', note: `my\nnotes\u0000${'x'.repeat(200)}`});
  assert.equal(tagged.source, 'coupon');
  assert.equal(tagged.note.length, 160);
  assert.ok(!/[\x00-\x1f\x7f]/.test(tagged.note));
  tagged.source = 'global';
  assert.equal(h.snapshot().events[0].source, 'coupon');
  assert.equal(h.tag(id, {source: 'automatic-guess'}), null);
  assert.equal(h.tag('missing', {source: 'global'}), null);
  assert.equal(h.tag(id, {source: 'global'}).source, 'global');
  assert.equal(h.tag(id, {source: 'scheduled'}).source, 'scheduled');
});

test('CSV safely quotes hostile labels and formula notes, and labels elapsed time', () => {
  const h = new UsageHistory();
  h.observe(bucket(10, 10000, {name: '=HYPERLINK("bad"), bad'}), 0);
  h.observe(bucket(90), 60000);
  h.tag(h.snapshot().events[0].id, {source: 'coupon', note: '\t+CMD|danger'});
  const csv = h.csv();
  assert.ok(csv.includes('"\'=HYPERLINK(""bad""), bad"'));
  assert.ok(csv.includes('"\'+CMD|danger"'));
  assert.ok(csv.includes('"elapsed_wall_clock_ms"'));
  assert.ok(csv.includes('"1970-01-01T00:00:00.000Z"'));
  assert.ok(csv.includes('"before_remaining_percent"'));
  assert.equal(csv.split('\r\n').filter(Boolean).length, 4);
  assert.ok(!csv.includes('active'));
});

test('gaps are flagged and use configurable expected intervals', () => {
  const h = new UsageHistory(null, {expectedIntervalMs: 30000});
  h.observe(bucket(20), 0);
  h.observe(bucket(15), 45000);
  assert.equal(h.snapshot().samples.at(-1).gap, false);
  h.observe(bucket(90), 91000);
  assert.equal(h.snapshot().samples.at(-1).elapsedMs, 46000);
  assert.equal(h.snapshot().samples.at(-1).gap, true);
  assert.equal(h.snapshot().events[0].gap, true);
  assert.equal(h.snapshot().events[0].source, 'unknown');
  h.observe(bucket(80), 200000, 90000);
  assert.equal(h.snapshot().samples.at(-1).gap, false);
});

test('clock rollback suppresses comparisons and flags non-increasing wall clock', () => {
  const h = new UsageHistory();
  h.observe(bucket(20), 60000);
  h.observe(bucket(90), 0);
  const sample = h.snapshot().samples.at(-1);
  assert.equal(sample.clockRollback, true);
  assert.equal(sample.gap, true);
  assert.equal(sample.elapsedMs, null);
  assert.equal(h.snapshot().events.length, 0);
});

test('sessions isolate baselines; disappearing buckets and clear cannot infer resets', () => {
  const h = new UsageHistory();
  h.observe(bucket(20), 0);
  const firstSession = h.sessionId;
  const nextSession = h.newSession();
  assert.notEqual(nextSession, firstSession);
  h.observe(bucket(90), 60000);
  assert.equal(h.snapshot().samples.length, 2);
  assert.equal(h.snapshot().events.length, 0);
  assert.equal(h.snapshot().samples.at(-1).elapsedMs, null);
  h.observe([], 120000);
  h.observe(bucket(100), 180000);
  assert.equal(h.snapshot().events.length, 0);
  h.clear();
  assert.deepEqual(h.snapshot(), {samples: [], events: []});
  h.observe(bucket(100), 240000);
  assert.equal(h.snapshot().events.length, 0);
});

test('multiple buckets and window kinds stay independent', () => {
  const h = new UsageHistory();
  const input = bucket(20);
  input[0].windows.push({kind: 'secondary', remaining: 90, durationMins: 300, resetsAt: 500});
  input.push({...bucket(90)[0], id: 'astra'});
  h.observe(input, 0);
  input[0].windows[0].remaining = 100;
  input[0].windows[1].remaining = 80;
  input[1].windows[0].remaining = 80;
  h.observe(input, 60000);
  assert.equal(h.snapshot().samples.length, 6);
  assert.equal(h.snapshot().events.length, 1);
  assert.equal(h.snapshot().events[0].bucket, 'sol');
  assert.equal(h.snapshot().events[0].window, 'primary');
});
