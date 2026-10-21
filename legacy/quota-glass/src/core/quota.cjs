'use strict';
const clean = (v, fallback = '') => typeof v === 'string' ? v.replace(/[\x00-\x1f\x7f]/g, '').slice(0, 160) : fallback;
const numeric = v => typeof v === 'number' && Number.isFinite(v);
function windowValue(value, kind) {
  if (!value || typeof value !== 'object') return null;
  return {kind, remaining: numeric(value.usedPercent) && value.usedPercent >= 0 && value.usedPercent <= 100 ? Number((100-value.usedPercent).toFixed(10)) : null,
    durationMins: numeric(value.windowDurationMins) && value.windowDurationMins > 0 ? value.windowDurationMins : null,
    resetsAt: numeric(value.resetsAt) && value.resetsAt > 0 ? value.resetsAt : null};
}
function normalize(payload) {
  if (!payload || typeof payload !== 'object') throw new Error('Invalid quota response');
  const multi = payload.rateLimitsByLimitId;
  const entries = multi && typeof multi === 'object' && !Array.isArray(multi) && Object.keys(multi).length ? Object.entries(multi) : payload.rateLimits ? [[payload.rateLimits.limitId || 'codex',payload.rateLimits]] : [];
  return entries.slice(0,100).filter(([,v]) => v && typeof v === 'object').map(([key,v]) => ({
    id: clean(v.limitId,clean(key,'unknown')), name: clean(v.limitName,clean(v.normalModelSlug,clean(v.limitId,clean(key,'Unknown bucket')))),
    windows: ['primary','secondary'].map(kind=>windowValue(v[kind],kind)).filter(Boolean),
    credits: v.credits && typeof v.credits === 'object' ? {hasCredits:v.credits.hasCredits === true,unlimited:v.credits.unlimited === true,balance: typeof v.credits.balance === 'string' && /^\d+(\.\d+)?$/.test(v.credits.balance) ? v.credits.balance.slice(0,80) : null} : null
  }));
}
function validateSettings(input) {
  return {refreshSeconds: Math.min(900,Math.max(30,Math.round(Number(input.refreshSeconds)||60))), alertThreshold: Math.min(100,Math.max(0,Math.round(Number(input.alertThreshold)||0))),alertsEnabled:input.alertsEnabled===true};
}
function demo(now=Date.now()) {
 return normalize({rateLimitsByLimitId:{sol:{limitName:'Sol · sample',primary:{usedPercent:21,windowDurationMins:10080,resetsAt:Math.floor(now/1000)+550800},credits:{hasCredits:true,unlimited:false,balance:'62416'}},luna:{limitName:'Luna · sample',primary:{usedPercent:8,windowDurationMins:10080,resetsAt:Math.floor(now/1000)+550800}},astra:{limitName:'Astra · sample',primary:{usedPercent:86,windowDurationMins:300,resetsAt:Math.floor(now/1000)+7200}}}});
}
class AlertTracker {
 constructor(){this.sent=new Set();}
 reset(){this.sent.clear();}
 check(buckets,threshold){
  const out=[];
  for(const b of buckets) for(const w of b.windows){
   const key=`${b.id}:${w.kind}:${w.resetsAt}:${threshold}`;
   if(w.remaining!==null && w.remaining<=threshold && !this.sent.has(key) && (w.resetsAt===null || w.resetsAt*1000>Date.now())){this.sent.add(key);out.push(`${b.name}: ${w.remaining}% remaining`);}
  }
  if(this.sent.size>1000)this.sent.clear();
  return out;
 }
}
module.exports={normalize,validateSettings,demo,AlertTracker};
