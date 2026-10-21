'use strict';
const {test}=require('node:test');
const assert=require('node:assert/strict');
const fs=require('node:fs');
const path=require('node:path');
const vm=require('node:vm');
const {EventEmitter}=require('node:events');

// Evaluate the actual main-process functions with inert Electron/filesystem/RPC
// boundaries. These regressions never spawn Codex or read local account data.
const mainPath=path.join(__dirname,'../src/main/main.cjs');
const flush=()=>new Promise(setImmediate);
function harness(){
  const clients=[],timers=new Map(),snapshots=[],handlers=new Map(),openedUrls=[],dialogs=[];let timerId=0;
  const fakeFs={
    files:new Map(),failWrite:false,failRename:false,
    writeFileSync(file,value){if(this.failWrite)throw new Error('Test disk write failure');this.files.set(file,String(value));},
    renameSync(from,to){if(this.failRename)throw new Error('Test disk rename failure');if(!this.files.has(from))throw new Error('Missing temporary file');this.files.set(to,this.files.get(from));this.files.delete(from);},
    statSync(file){return {size:Buffer.byteLength(this.readFileSync(file))};},
    readFileSync(file){if(!this.files.has(file))throw new Error('No stored test data');return this.files.get(file);},
    mkdirSync(){},existsSync(file){return this.files.has(file);},
  };
  class FakeRpc extends EventEmitter {
    constructor(){super();this.pending=[];this.account={type:'chatgpt',email:'old@example.test'};this.stopped=false;clients.push(this);}
    start(){return Promise.resolve();}
    stop(){this.stopped=true;}
    request(method,params){
      if(method==='account/read')return Promise.resolve({account:this.account});
      return new Promise((resolve,reject)=>this.pending.push({method,params,resolve,reject}));
    }
    next(method='account/rateLimits/read'){
      const index=this.pending.findIndex(request=>request.method===method);
      assert.notEqual(index,-1,`Expected pending ${method}`);
      return this.pending.splice(index,1)[0];
    }
  }
  class FakeWindow extends EventEmitter {
    constructor(){super();this.webContents=new EventEmitter();Object.assign(this.webContents,{mainFrame:{},send:(_channel,value)=>snapshots.push(JSON.parse(JSON.stringify(value))),setWindowOpenHandler(){}});}
    isDestroyed(){return false;}
    loadFile(){}
    show(){}
    hide(){}
    focus(){}
  }
  class FakeTray extends EventEmitter {setTitle(){}setToolTip(){}setContextMenu(){}}
  const electron={
    app:Object.assign(new EventEmitter(),{requestSingleInstanceLock:()=>true,quit(){},getPath:()=>'/quota-glass-test',whenReady:()=>Promise.resolve(),setAppUserModelId(){}}),
    BrowserWindow:FakeWindow,Tray:FakeTray,Menu:{buildFromTemplate:items=>items},
    nativeImage:{createFromPath:()=>({setTemplateImage(){}})},
    ipcMain:{handle:(name,handler)=>handlers.set(name,handler)},
    session:{defaultSession:{setPermissionRequestHandler(){},setPermissionCheckHandler(){},webRequest:{onBeforeRequest(){}}}},
    powerMonitor:new EventEmitter(),Notification:{isSupported:()=>false},
    dialog:{showMessageBox:async(_window,options)=>{dialogs.push(options);return {response:1};}},
    shell:{openExternal:async url=>openedUrls.push(url)},
  };
  const context={
    console,URL,queueMicrotask,Buffer,
    process:{platform:'linux',argv:[]},__dirname:path.dirname(mainPath),
    setTimeout:(fn,ms)=>{timers.set(++timerId,{fn,ms});return timerId;},
    clearTimeout:id=>timers.delete(id),
    setInterval:()=>({unref(){}}),
    require:id=>{
      if(id==='electron')return electron;
      if(id==='node:fs')return fakeFs;
      if(id==='../core/rpc.cjs')return {RpcClient:FakeRpc};
      return require(id.startsWith('.')?path.resolve(path.dirname(mainPath),id):id);
    },
  };
  vm.createContext(context);
  vm.runInContext(fs.readFileSync(mainPath,'utf8')+`
    globalThis.api={connect,refresh,stop,login,getState:()=>state,getWindow:()=>win};
  `,context,{filename:mainPath});
  const invoke=(name,...args)=>{const contents=context.api.getWindow().webContents;return handlers.get(name)({sender:contents,senderFrame:contents.mainFrame},...args);};
  return {api:context.api,clients,timers,snapshots,openedUrls,dialogs,fs:fakeFs,invoke};
}
const quota=name=>({rateLimits:{limitId:'codex',limitName:name,primary:{usedPercent:11,windowDurationMins:300,resetsAt:2000000000}}});

test('account change discards an in-flight old quota and queues a fresh read',async()=>{
  const h=harness();const connecting=h.api.connect();await flush();
  const rpc=h.clients[0],old=rpc.next();
  rpc.account={type:'chatgpt',email:'new@example.test'};
  rpc.emit('notification','account/updated',{});
  old.resolve(quota('Old account'));
  await connecting;await flush();
  assert.equal(h.snapshots.some(s=>s.buckets.some(b=>b.name==='Old account')),false);
  rpc.next().resolve(quota('New account'));await flush();
  assert.equal(h.api.getState().status,'live');
  assert.equal(h.api.getState().buckets[0].name,'New account');
  h.api.stop();
});

test('disconnect during refresh schedules a new client rather than polling the dead one',async()=>{
  const h=harness();const connecting=h.api.connect();await flush();
  const rpc=h.clients[0];rpc.emit('disconnected','Test connection closed');
  rpc.next().reject(new Error('Test connection closed'));
  await connecting;await flush();
  assert.equal(h.api.getState().status,'offline');
  assert.equal(h.timers.size,1);
  const [{fn}]=h.timers.values();fn();await flush();
  assert.equal(h.clients.length,2,'Reconnect must construct a fresh RPC transport');
  assert.equal(rpc.stopped,true);
  h.clients[1].next().resolve(quota('Reconnected'));await flush();
  assert.equal(h.api.getState().status,'live');
  h.api.stop();
});

test('a stopped connection cannot publish late quota results',async()=>{
  const h=harness();const connecting=h.api.connect();await flush();
  const pending=h.clients[0].next();h.api.stop();
  pending.resolve(quota('Stale connection'));await connecting;await flush();
  assert.equal(h.snapshots.some(s=>s.buckets.some(b=>b.name==='Stale connection')),false);
  assert.equal(h.timers.size,0);
});

test('cancel while login starts cancels its late ID without opening the browser',async()=>{
  const h=harness();const connecting=h.api.connect();await flush();
  const rpc=h.clients[0];rpc.next().resolve(quota('Connected'));await connecting;
  const signingIn=h.api.login();await flush();
  const start=rpc.next('account/login/start');
  await h.invoke('cancel-login');
  const cancelledMessage=h.api.getState().message;
  start.resolve({loginId:'cancelled-attempt',authUrl:'https://auth.openai.com/test-login'});
  await flush();
  const cancel=rpc.next('account/login/cancel');
  assert.equal(cancel.params.loginId,'cancelled-attempt');
  cancel.resolve({});await signingIn;
  assert.equal(h.openedUrls.length,0);
  assert.equal(h.api.getState().loginPending,false);
  assert.equal(h.api.getState().message,cancelledMessage);
  h.api.stop();
});

test('late login failure cannot overwrite demo state after disconnecting',async()=>{
  const h=harness();const connecting=h.api.connect();await flush();
  const rpc=h.clients[0];rpc.next().resolve(quota('Connected'));await connecting;
  const signingIn=h.api.login();await flush();
  const start=rpc.next('account/login/start');await h.invoke('demo',true);
  const demoMessage=h.api.getState().message;
  start.reject(new Error('Connection stopped'));await signingIn;
  assert.equal(h.api.getState().status,'demo');
  assert.equal(h.api.getState().message,demoMessage);
  assert.equal(h.api.getState().loginPending,false);
  assert.equal(h.openedUrls.length,0);
});

test('late cancellation acknowledgement cannot overwrite a newer demo state',async()=>{
  const h=harness();const connecting=h.api.connect();await flush();
  const rpc=h.clients[0];rpc.next().resolve(quota('Connected'));await connecting;
  const signingIn=h.api.login();await flush();
  rpc.next('account/login/start').resolve({loginId:'active-attempt',authUrl:'https://auth.openai.com/test-login'});
  await signingIn;
  assert.equal(h.openedUrls.length,1);
  const cancelling=h.invoke('cancel-login');await flush();
  const cancel=rpc.next('account/login/cancel');await h.invoke('demo',true);
  const demoMessage=h.api.getState().message;
  cancel.resolve({});await cancelling;
  assert.equal(h.api.getState().status,'demo');
  assert.equal(h.api.getState().message,demoMessage);
  assert.equal(h.api.getState().loginPending,false);
});

test('an untrusted sign-in URL is never opened and its login is cancelled',async()=>{
  const h=harness();const connecting=h.api.connect();await flush();
  const rpc=h.clients[0];rpc.next().resolve(quota('Connected'));await connecting;
  const signingIn=h.api.login();await flush();
  rpc.next('account/login/start').resolve({loginId:'bad-url',authUrl:'https://auth.openai.com.attacker.example/login'});
  await flush();rpc.next('account/login/cancel').resolve({});await signingIn;
  assert.equal(h.openedUrls.length,0);
  assert.equal(h.api.getState().loginPending,false);
  h.api.stop();
});

test('an old login completion does not clear a newer pending login',async()=>{
  const h=harness();const connecting=h.api.connect();await flush();
  const rpc=h.clients[0];rpc.next().resolve(quota('Connected'));await connecting;
  const first=h.api.login();await flush();
  rpc.next('account/login/start').resolve({loginId:'first-login',authUrl:'https://auth.openai.com/test-login'});await first;
  const cancelling=h.invoke('cancel-login');await flush();
  rpc.next('account/login/cancel').resolve({});await cancelling;
  const second=h.api.login();await flush();
  rpc.next('account/login/start').resolve({loginId:'second-login',authUrl:'https://auth.openai.com/test-login'});await second;
  rpc.emit('notification','account/login/completed',{loginId:'first-login',success:false});
  assert.equal(h.api.getState().loginPending,true,'Stale completions must not dismiss the current login');
  const cancelSecond=h.invoke('cancel-login');await flush();
  const request=rpc.next('account/login/cancel');assert.equal(request.params.loginId,'second-login');request.resolve({});await cancelSecond;
  h.api.stop();
});

for(const failure of ['failWrite','failRename'])test(`history ${failure} preserves live quotas and shows a separate persistence error`,async()=>{
  const h=harness();const connecting=h.api.connect();await flush();
  const rpc=h.clients[0];rpc.next().resolve(quota('Connected'));await connecting;
  const filename=path.join('/quota-glass-test','quota-history.json'),stored=h.fs.files.get(filename);
  assert.ok(stored);
  h.fs[failure]=true;
  const refreshing=h.api.refresh();await flush();rpc.next().resolve(quota('Latest quotas'));await refreshing;
  assert.equal(h.api.getState().status,'live');
  assert.equal(h.api.getState().buckets[0].name,'Latest quotas');
  assert.equal(typeof h.api.getState().historyError,'string');
  assert.ok(h.api.getState().history.samples.length>JSON.parse(stored).samples.length);
  assert.equal(h.fs.files.get(filename),stored,'Previously persisted file must survive a failed save');
  h.fs[failure]=false;
  const retry=h.api.refresh();await flush();rpc.next().resolve(quota('Recovered'));await retry;
  assert.equal(h.api.getState().historyError,null);
  assert.equal(JSON.parse(h.fs.files.get(filename)).samples.length,h.api.getState().history.samples.length);
  h.api.stop();
});

test('failed history clear restores memory and leaves stored records intact',async()=>{
  const h=harness();const connecting=h.api.connect();await flush();
  h.clients[0].next().resolve(quota('Connected'));await connecting;
  const filename=path.join('/quota-glass-test','quota-history.json'),stored=h.fs.files.get(filename);
  const previous=JSON.parse(JSON.stringify(h.api.getState().history));
  h.fs.failRename=true;await h.invoke('clear-history');
  assert.deepEqual(h.api.getState().history,previous);
  assert.equal(h.fs.files.get(filename),stored);
  assert.equal(typeof h.api.getState().historyError,'string');
  assert.ok(h.dialogs.some(dialog=>dialog.type==='error'));
  h.fs.failRename=false;await h.invoke('clear-history');
  assert.equal(h.api.getState().history.samples.length,0);
  assert.equal(h.api.getState().history.events.length,0);
  assert.equal(JSON.parse(h.fs.files.get(filename)).samples.length,0);
  assert.equal(h.api.getState().historyError,null);
  h.api.stop();
});

test('demo state never exposes or appends the real stored history',async()=>{
  const h=harness();const connecting=h.api.connect();await flush();
  h.clients[0].next().resolve(quota('Private account'));await connecting;
  const filename=path.join('/quota-glass-test','quota-history.json'),stored=h.fs.files.get(filename);
  const count=h.api.getState().history.samples.length;assert.ok(count>0);
  await h.invoke('demo',true);await h.api.refresh();
  assert.equal(h.api.getState().history.samples.length,0);
  assert.equal(h.api.getState().history.events.length,0);
  assert.equal(h.fs.files.get(filename),stored);
  await h.invoke('demo',false);
  assert.equal(h.api.getState().history.samples.length,count,'Leaving demo restores the independent real history');
  h.api.stop();
});

test('failed manual tag save rolls back the tag and returns an error',async()=>{
  const h=harness();const connecting=h.api.connect();await flush();const rpc=h.clients[0];rpc.next().resolve(quota('QA'));await connecting;
  await new Promise(resolve=>setTimeout(resolve,5));const updated=h.api.refresh();await flush();const increased=quota('QA');increased.rateLimits.primary.usedPercent=0;rpc.next().resolve(increased);await updated;
  const event=h.api.getState().history.events[0];assert.ok(event);
  h.fs.failRename=true;const result=await h.invoke('tag-reset',event.id,{source:'coupon',note:'test'});
  assert.equal(typeof result.error,'string');assert.equal(h.api.getState().history.events[0].source,'unknown');
  h.fs.failRename=false;const saved=await h.invoke('tag-reset',event.id,{source:'coupon',note:'test'});
  assert.equal(saved.history.events[0].source,'coupon');h.api.stop();
});

test('demo blocks history mutation and export in main process',async()=>{
  const h=harness();await flush();await h.invoke('demo',true);
  const before=h.dialogs.length;
  for(const [name,args] of [['tag-reset',['id',{source:'coupon'}]],['clear-history',[]],['export-history',[]]]){
    const result=await h.invoke(name,...args);assert.equal(typeof result.error,'string');
  }
  assert.equal(h.dialogs.length,before);h.api.stop();
});

