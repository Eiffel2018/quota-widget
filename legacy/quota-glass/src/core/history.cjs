'use strict';
const {randomUUID} = require('node:crypto');

const MAX_SAMPLES = 3000;
const MAX_EVENTS = 100;
const SOURCES = new Set(['coupon', 'global', 'scheduled', 'unknown']);
const REASONS = new Set(['remaining-increase', 'reset-time-changed', 'balance-increase']);
const clean = (value, max = 160) => typeof value === 'string'
  ? value.replace(/[\x00-\x1f\x7f]/g, ' ').trim().slice(0, max) : '';
const record = value => value && typeof value === 'object' && !Array.isArray(value);
const finite = value => typeof value === 'number' && Number.isFinite(value);
const positive = value => finite(value) && value > 0 ? value : null;
const amount = value => finite(value) && value >= 0 && value <= Number.MAX_SAFE_INTEGER ? value : null;
const percent = value => finite(value) && value >= 0 && value <= 100 ? value : null;
const timestamp = value => finite(value) && value >= 0 && value <= 8640000000000000;
const interval = value => finite(value) && value > 0 ? value : 60000;
const clone = value => JSON.parse(JSON.stringify(value));

function values(value) {
  if (!record(value)) return null;
  return {remaining: percent(value.remaining), balance: amount(value.balance),
    durationMins: positive(value.durationMins), resetsAt: positive(value.resetsAt)};
}

function creditValue(value) {
  if (!record(value)) return null;
  const balance = typeof value.balance === 'string' && /^\d+(\.\d+)?$/.test(value.balance)
    ? amount(Number(value.balance)) : amount(value.balance);
  return {hasCredits: value.hasCredits === true, unlimited: value.unlimited === true,
    balance: value.unlimited === true ? null : balance};
}

function common(value) {
  if (!record(value) || !timestamp(value.timestamp)) return null;
  const bucket = clean(value.bucket), window = clean(value.window, 40);
  if (!bucket || !['primary', 'secondary', 'credits'].includes(window)) return null;
  return {timestamp: value.timestamp, sessionId: clean(value.sessionId, 80) || 'legacy',
    bucket, bucketName: clean(value.bucketName) || bucket, window,
    previousTimestamp: timestamp(value.previousTimestamp) ? value.previousTimestamp : null,
    elapsedMs: amount(value.elapsedMs), gap: value.gap === true,
    clockRollback: value.clockRollback === true};
}

function sampleValue(value) {
  const base = common(value);
  return base ? {...base, ...values(value), credits: creditValue(value.credits)} : null;
}

function eventValue(value) {
  const base = common(value);
  if (!base || !clean(value.id, 80) || !record(value.before) || !record(value.after)) return null;
  return {...base, id: clean(value.id, 80), before: values(value.before), after: values(value.after),
    source: SOURCES.has(value.source) ? value.source : 'unknown', note: clean(value.note),
    reasons: Array.isArray(value.reasons) ? [...new Set(value.reasons.filter(v => REASONS.has(v)))] : []};
}

function restore(list, limit, normalize) {
  if (!Array.isArray(list)) return [];
  return list.slice(-limit).map(normalize).filter(Boolean);
}

// Quote every cell, and neutralize formula prefixes even after leading whitespace.
function csvCell(value) {
  let text = value === null || value === undefined ? '' : String(value);
  if (/^[\s\uFEFF]*[=+\-@]/u.test(text)) text = `'${text}`;
  return `"${text.replace(/"/g, '""')}"`;
}

/**
 * Bounded, local observations. This does not identify an account, measure active
 * usage time, or establish why a reported allowance changed. Epoch timestamps
 * are milliseconds; resetsAt is the API's epoch-seconds value.
 */
class UsageHistory {
  constructor(priorState = null, {expectedIntervalMs = 60000} = {}) {
    const state = record(priorState) ? priorState : {};
    this.samples = restore(state.samples, MAX_SAMPLES, sampleValue);
    this.events = restore(state.events, MAX_EVENTS, eventValue);
    this.expectedIntervalMs = interval(expectedIntervalMs);
    this.baselines = new Map();
    this.newSession();
  }

  // A new process/reconnect never infers a reset against a previous session.
  newSession() {
    this.sessionId = randomUUID();
    this.baselines.clear();
    return this.sessionId;
  }

  clear() {
    this.samples = [];
    this.events = [];
    this.newSession();
  }

  observe(buckets, now = Date.now(), expectedIntervalMs = this.expectedIntervalMs) {
    if (!timestamp(now) || !Array.isArray(buckets)) return this.snapshot();
    const expected = interval(expectedIntervalMs);
    const seen = new Set();
    for (const bucket of buckets.slice(0, 100)) {
      if (!record(bucket)) continue;
      const id = clean(bucket.id);
      if (!id) continue;
      const credits = creditValue(bucket.credits);
      const windows = Array.isArray(bucket.windows) ? bucket.windows.slice(0, 2) : [];
      const observations = windows.filter(w => record(w) && ['primary', 'secondary'].includes(w.kind))
        .map(w => ({window: w.kind, ...values(w)}));
      if (credits) observations.push({window: 'credits', remaining: null, balance: credits.balance,
        durationMins: null, resetsAt: null});
      for (const observation of observations) {
        const key = JSON.stringify([id, observation.window]);
        if (seen.has(key)) continue;
        seen.add(key);
        const previous = this.baselines.get(key);
        const elapsed = previous ? now - previous.timestamp : null;
        const clockRollback = previous ? elapsed <= 0 : false;
        const sample = {timestamp: now, sessionId: this.sessionId, bucket: id,
          bucketName: clean(bucket.name) || id, ...observation, credits,
          previousTimestamp: previous ? previous.timestamp : null,
          elapsedMs: clockRollback ? null : elapsed,
          gap: previous ? clockRollback || elapsed > expected * 1.5 : false, clockRollback};
        this.samples.push(sample);
        if (previous && !clockRollback) {
          const reasons = [];
          if (sample.remaining !== null && previous.remaining !== null && sample.remaining > previous.remaining)
            reasons.push('remaining-increase');
          // Null-to-known is newly available data, not evidence of a reset.
          if (sample.resetsAt !== null && previous.resetsAt !== null && sample.resetsAt !== previous.resetsAt)
            reasons.push('reset-time-changed');
          if (sample.balance !== null && previous.balance !== null && sample.balance > previous.balance)
            reasons.push('balance-increase');
          if (reasons.length) this.events.push({...common(sample), id: randomUUID(),
            before: values(previous), after: values(sample), source: 'unknown', note: '', reasons});
        }
        this.baselines.set(key, sample);
      }
    }
    // Disappearance is missing data; don't compare a reappearing bucket to it.
    for (const key of this.baselines.keys()) if (!seen.has(key)) this.baselines.delete(key);
    this.samples = this.samples.slice(-MAX_SAMPLES);
    this.events = this.events.slice(-MAX_EVENTS);
    return this.snapshot();
  }

  tag(id, {source = 'unknown', note = ''} = {}) {
    if (!SOURCES.has(source)) return null;
    const event = this.events.find(item => item.id === id);
    if (!event) return null;
    event.source = source;
    event.note = clean(note);
    return clone(event);
  }

  snapshot() { return clone({samples: this.samples, events: this.events}); }
  toJSON() { return this.snapshot(); }

  csv() {
    const header = ['record_type', 'timestamp_ms', 'timestamp_utc', 'session_id', 'bucket', 'bucket_name',
      'window', 'remaining_percent', 'credit_balance', 'duration_minutes', 'resets_at_seconds',
      'previous_timestamp_ms', 'elapsed_wall_clock_ms', 'gap', 'clock_rollback', 'event_id', 'source', 'note',
      'reasons', 'before_remaining_percent', 'after_remaining_percent', 'before_credit_balance',
      'after_credit_balance', 'before_resets_at_seconds', 'after_resets_at_seconds'];
    const rows = [header];
    for (const sample of this.samples) rows.push(['sample', sample.timestamp, new Date(sample.timestamp).toISOString(),
      sample.sessionId, sample.bucket, sample.bucketName, sample.window, sample.remaining, sample.balance,
      sample.durationMins, sample.resetsAt, sample.previousTimestamp, sample.elapsedMs, sample.gap,
      sample.clockRollback, '', '', '', '', '', '', '', '', '', '']);
    for (const event of this.events) rows.push(['event', event.timestamp, new Date(event.timestamp).toISOString(),
      event.sessionId, event.bucket, event.bucketName, event.window, '', '', '', '', event.previousTimestamp,
      event.elapsedMs, event.gap, event.clockRollback, event.id, event.source, event.note, event.reasons.join(';'),
      event.before.remaining, event.after.remaining, event.before.balance, event.after.balance,
      event.before.resetsAt, event.after.resetsAt]);
    return `${rows.map(row => row.map(csvCell).join(',')).join('\r\n')}\r\n`;
  }
}

module.exports = {UsageHistory, MAX_SAMPLES, MAX_EVENTS};
