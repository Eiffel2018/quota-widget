'use strict';
const {EventEmitter}=require('node:events');
const {spawn}=require('node:child_process');
// Only quota/account methods. Never send prompts or start work, spend credits, or read tokens.
const ALLOWED=new Set(['initialize','account/read','account/rateLimits/read','account/login/start','account/login/cancel']);
class RpcClient extends EventEmitter {
 constructor(binary,options={}) { super();this.binary=binary;this.spawn=options.spawn||spawn;this.timeoutMs=options.timeoutMs||20000;this.pending=new Map();this.counter=0;this.buffer='';this.child=null;this.stopped=false; }
 async start(){
  this.stopped=false;
  this.child=this.spawn(this.binary,['app-server','--listen','stdio://'],{stdio:['pipe','pipe','pipe'],shell:false,windowsHide:true});
  this.child.stdout.setEncoding('utf8');
  this.child.stdout.on('data',chunk=>this.receive(chunk));
  // Never persist or relay child stderr, which might include personal data.
  this.child.stderr.resume();
  for(const stream of [this.child.stdin,this.child.stdout,this.child.stderr])stream.on('error',()=>{this.fail('Codex connection interrupted. Reconnect to try again.');this.stop();});
  this.child.on('error',()=>this.fail('Could not start Codex. Choose an installed official Codex executable.'));
  this.child.on('exit',()=>this.fail('Codex connection closed. Reconnect to try again.'));
  await this.request('initialize',{clientInfo:{name:'quota_glass',title:'Quota Glass',version:'0.1.0'}});
  this.write({method:'initialized'});
 }
 write(value){if(!this.child || !this.child.stdin.writable)throw new Error('Codex is not connected');this.child.stdin.write(JSON.stringify(value)+'\n');}
 request(method,params={}){
  if(!ALLOWED.has(method))return Promise.reject(new Error('Method not allowed'));
  const id=++this.counter;
  return new Promise((resolve,reject)=>{const timer=setTimeout(()=>{this.pending.delete(id);reject(new Error('Codex request timed out. Check your connection.'));},this.timeoutMs);this.pending.set(id,{resolve,reject,timer});try{this.write({id,method,params});}catch{clearTimeout(timer);this.pending.delete(id);reject(new Error('Codex is not connected'));}});
 }
 receive(chunk){
  this.buffer+=chunk;
  if(this.buffer.length>2*1024*1024){this.fail('Codex returned an oversized response.');this.stop();return;}
  let end;
  while((end=this.buffer.indexOf('\n'))>=0){const line=this.buffer.slice(0,end);this.buffer=this.buffer.slice(end+1);let value;try{value=JSON.parse(line);}catch{continue;}
   if(!value || typeof value!=='object' || Array.isArray(value))continue;
   if(value.id!==undefined && value.method){try{this.write({id:value.id,error:{code:-32601,message:'Unsupported client request'}});}catch{}continue;}
   if(value.id!==undefined){const p=this.pending.get(value.id);if(!p)continue;clearTimeout(p.timer);this.pending.delete(value.id);if(value.error)p.reject(new Error('Codex request failed. Check sign-in and CLI version.'));else p.resolve(value.result);}
   else if(typeof value.method==='string')this.emit('notification',value.method,value.params);
  }
 }
 fail(message){for(const p of this.pending.values()){clearTimeout(p.timer);p.reject(new Error(message));}this.pending.clear();if(!this.stopped)this.emit('disconnected',message);}
 stop(){this.stopped=true;this.fail('Connection stopped');this.child?.kill();this.child=null;this.buffer='';}
}
module.exports={RpcClient};
