'use strict';
// Local, in-memory, bounded 15-minute observations. No interpolation or invented source precision.
class RateHistory {
 constructor({windowMs=15*60000,minSpanMs=60000,staleMs=180000}={}){this.windowMs=windowMs;this.minSpanMs=minSpanMs;this.staleMs=staleMs;this.series=new Map();}
 clear(){this.series.clear();}
 observe(key,value,segment,now){
  if(typeof value!=='number'||!Number.isFinite(value)){this.series.delete(key);return null;}
  let entry=this.series.get(key);
  if(!entry||entry.segment!==segment||!entry.points.length||value>entry.points.at(-1).value||now-entry.points.at(-1).at>this.staleMs||now<=entry.points.at(-1).at){entry={segment,points:[]};this.series.set(key,entry);}
  entry.points.push({at:now,value});
  entry.points=entry.points.filter(p=>p.at>=now-this.windowMs).slice(-1000);
  return this.read(key,now);
 }
 read(key,now){const entry=this.series.get(key);if(!entry||entry.points.length<2)return null;const first=entry.points[0],last=entry.points.at(-1);const span=last.at-first.at;
  if(span<this.minSpanMs||now-last.at>this.staleMs)return null;
  const perMinute=(first.value-last.value)/(span/60000);
  return {perMinute,observedMinutes:span/60000,etaMinutes:perMinute>0?last.value/perMinute:null};
 }
 apply(buckets,now=Date.now()){
  const keys=new Set();
  for(const b of buckets){for(const w of b.windows){const key=JSON.stringify([b.id,w.kind]);keys.add(key);w.rate=this.observe(key,w.remaining,`${w.resetsAt}:${w.durationMins}`,now);}
   if(b.credits){const key=JSON.stringify([b.id,'credits']);keys.add(key);const number=Number(b.credits.balance);const amount=b.credits.unlimited||b.credits.balance===null||!Number.isFinite(number)||number>Number.MAX_SAFE_INTEGER?null:number;b.credits.rate=this.observe(key,amount,'credits',now);}
  }
  for(const key of this.series.keys())if(!keys.has(key))this.series.delete(key);
  return buckets;
 }
}
module.exports={RateHistory};
