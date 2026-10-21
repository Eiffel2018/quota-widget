'use strict';
const {app,BrowserWindow,Tray,Menu,nativeImage,ipcMain,dialog,Notification,shell,powerMonitor,session}=require('electron');
const path=require('node:path');const fs=require('node:fs');const os=require('node:os');
const {RpcClient}=require('../core/rpc.cjs');const {normalize,validateSettings,demo,AlertTracker}=require('../core/quota.cjs');
let win,tray,client,timer,loginId=null,quitting=false,busy=false,generation=0,binary=process.platform==='win32'?'codex.exe':'codex',autoConnect=false,disconnected=false,accountEpoch=0,refreshQueued=false,loginAttempt=0;
const {RateHistory}=require('../core/rates.cjs');const {createHash}=require('node:crypto');const rates=new RateHistory();let accountFingerprint=null;
const {UsageHistory}=require('../core/history.cjs');let history=new UsageHistory();
const alerts=new AlertTracker();
let state={status:'setup',message:'連接本機 Codex，或先查看示範資料。',lastRefresh:null,buckets:[],settings:{refreshSeconds:60,alertThreshold:20,alertsEnabled:false},binaryLabel:'PATH 中的 Codex',loginPending:false};
const historyFile=()=>path.join(app.getPath('userData'),'quota-history.json');
function saveHistory(){try{const temp=historyFile()+'.tmp';fs.writeFileSync(temp,JSON.stringify(history.snapshot()),{mode:0o600});fs.renameSync(temp,historyFile());state.historyError=null;return true;}catch{state.historyError='無法儲存本機歷史；目前只保留在記憶體。';return false;}}
const prefsFile=()=>path.join(app.getPath('userData'),'preferences.json');
function save(){try{fs.writeFileSync(prefsFile(),JSON.stringify({settings:state.settings,binary,autoConnect}),{mode:0o600});}catch{state.message='無法儲存設定；本次執行仍可使用。';}}
function emit(){state.history=state.status==='demo'?{samples:[],events:[]}:history.snapshot();if(win&&!win.isDestroyed())win.webContents.send('state-update',state);updateTray();}
function updateTray(){if(!tray)return;const stale=state.lastRefresh&&Date.now()-state.lastRefresh>state.settings.refreshSeconds*2000;const nums=state.buckets.flatMap(b=>b.windows.map(w=>w.remaining)).filter(n=>n!==null);const min=nums.length?Math.min(...nums):null;const label=state.status==='live'&&!stale&&min!==null?`${Math.round(min)}%`:state.status==='demo'?'DEMO':state.status==='setup'?'—':'!';if(process.platform==='darwin')tray.setTitle(label);tray.setToolTip(`Quota Glass · ${state.status}${stale?' · stale':''}${min!==null?` · lowest window ${Math.round(min)}%`:''}`);tray.setContextMenu(Menu.buildFromTemplate([{label:'開啟 Quota Glass',click:show},{label:'重新整理',enabled:!busy&&state.status!=='setup',click:()=>refresh()},{type:'separator'},{label:'退出',click:()=>app.quit()}]));}
function show(){win.show();win.focus();}
function stop(){history.newSession();generation++;loginAttempt++;accountEpoch++;refreshQueued=false;disconnected=false;rates.clear();accountFingerprint=null;clearTimeout(timer);client?.stop();client=null;loginId=null;state.loginPending=false;busy=false;}
function schedule(){clearTimeout(timer);if(client)timer=setTimeout(()=>disconnected?connect():refresh(),state.settings.refreshSeconds*1000);}
async function connect(){stop();const gen=generation;state={...state,status:'connecting',message:'正在連接本機 Codex…',buckets:[],lastRefresh:null};emit();const rpc=new RpcClient(binary);client=rpc;
 rpc.on('disconnected',message=>{if(gen!==generation)return;disconnected=true;state.status='offline';state.message=message;state.loginPending=false;emit();clearTimeout(timer);timer=setTimeout(()=>{if(gen===generation)connect();},Math.max(30,state.settings.refreshSeconds)*1000);});
 rpc.on('notification',(method,params)=>{if(gen!==generation)return;if(method==='account/rateLimits/updated'){// Notifications can be sparse. Fetch the complete multi-bucket snapshot.
 refresh();}else if(method==='account/updated'){history.newSession();accountEpoch++;refreshQueued=true;state.buckets=[];state.lastRefresh=null;alerts.reset();rates.clear();accountFingerprint=null;emit();refresh();}else if(method==='account/login/completed'){if(!loginId||params?.loginId!==loginId)return;loginId=null;state.loginPending=false;state.message=params?.success?'已登入，正在更新…':'登入已取消或未成功。';emit();if(params?.success)refresh();}});
 try{await rpc.start();if(gen!==generation)return;autoConnect=true;save();await refresh();}catch(e){if(gen!==generation)return;state.status='offline';state.message=e.message;emit();clearTimeout(timer);timer=setTimeout(()=>{if(gen===generation)connect();},Math.max(30,state.settings.refreshSeconds)*1000);}}
async function refresh(){if(busy){refreshQueued=true;return state;}if(state.status==='demo'){state.lastRefresh=Date.now();emit();return state;}if(!client){await connect();return state;}busy=true;const rpc=client,gen=generation,epoch=accountEpoch;refreshQueued=false;
 try{const account=await rpc.request('account/read',{refreshToken:false});if(gen!==generation||epoch!==accountEpoch)return state;
  if(!account?.account||account.account.type!=='chatgpt'){state.status='auth-required';state.message='需要本機 ChatGPT 登入；API 金鑰帳戶不提供方案配額。';state.buckets=[];state.lastRefresh=null;alerts.reset();return state;}
  const fingerprint=createHash('sha256').update(JSON.stringify(account.account)).digest('hex');if(accountFingerprint!==fingerprint){history.newSession();rates.clear();alerts.reset();state.buckets=[];state.lastRefresh=null;accountFingerprint=fingerprint;}
  const payload=await rpc.request('account/rateLimits/read');if(gen!==generation||epoch!==accountEpoch)return state;rates.staleMs=Math.max(180000,state.settings.refreshSeconds*2000);state.buckets=rates.apply(normalize(payload));state.lastRefresh=Date.now();history.observe(state.buckets,state.lastRefresh,state.settings.refreshSeconds*1000);saveHistory();state.status='live';state.message=state.buckets.length?'由本機 Codex app-server 回報。':'來源未有回傳此帳戶的配額。';
  if(state.settings.alertsEnabled&&Notification.isSupported())for(const body of alerts.check(state.buckets,state.settings.alertThreshold))new Notification({title:'配額即將用盡',body}).show();
 }catch(e){if(gen===generation){state.status='offline';state.message=e.message;}}
 finally{if(gen===generation){busy=false;emit();if(refreshQueued&&!disconnected){refreshQueued=false;queueMicrotask(()=>refresh());}else schedule();}}return state;}
async function login(){
 if(state.loginPending)return state;
 if(!client)await connect();if(!client||disconnected)return state;
 const rpc=client,gen=generation,attempt=++loginAttempt;state.loginPending=true;emit();
 try{
  const choice=await dialog.showMessageBox(win,{type:'question',buttons:['取消','前往 ChatGPT'],defaultId:0,cancelId:0,title:'登入 ChatGPT',message:'允許 Codex 登入並在本機保存登入狀態？',detail:'官方 Codex 程式會在瀏覽器開啟 ChatGPT，並管理本機登入。這可能改變其他本機 Codex 用戶端使用的帳戶。Quota Glass 不會取得密碼或權杖。請在瀏覽器檢查並批准存取。此工具並非 OpenAI 官方產品。'});
  if(gen!==generation||attempt!==loginAttempt)return state;
  if(choice.response!==1){state.loginPending=false;emit();return state;}
  const result=await rpc.request('account/login/start',{type:'chatgpt'});
  if(gen!==generation||attempt!==loginAttempt){if(result?.loginId)await rpc.request('account/login/cancel',{loginId:result.loginId}).catch(()=>{});return state;}
  loginId=result.loginId;const url=new URL(result.authUrl);
  if(url.protocol!=='https:'||!['auth.openai.com','chatgpt.com','auth.chatgpt.com'].includes(url.hostname)||url.username||url.password)throw new Error('Unexpected sign-in URL');
  await shell.openExternal(url.href);if(gen!==generation||attempt!==loginAttempt)return state;state.message='請在瀏覽器完成登入，或在此取消。';
 }catch{
  if(gen!==generation||attempt!==loginAttempt)return state;
  if(loginId)await rpc.request('account/login/cancel',{loginId}).catch(()=>{});
  if(gen!==generation||attempt!==loginAttempt)return state;
  loginId=null;state.loginPending=false;state.message='無法啟動登入。請嘗試透過官方 Codex CLI 登入。';
 }emit();return state;
}
function handle(name,fn){ipcMain.handle(name,async(event,...args)=>{if(event.sender!==win.webContents||event.senderFrame!==win.webContents.mainFrame)throw new Error('Unauthorized frame');return fn(...args);});}
if(!app.requestSingleInstanceLock())app.quit();else{
 app.on('second-instance',()=>win&&show());app.on('before-quit',()=>{quitting=true;stop();});
 app.whenReady().then(()=>{
  app.setAppUserModelId('io.quotaglass.desktop');
  try{if(fs.statSync(historyFile()).size<=4*1024*1024)history=new UsageHistory(JSON.parse(fs.readFileSync(historyFile(),'utf8')));}catch{}
  try{const p=JSON.parse(fs.readFileSync(prefsFile(),'utf8'));state.settings=validateSettings(p.settings||state.settings);if(typeof p.binary==='string'&&(path.isAbsolute(p.binary)||['codex','codex.exe'].includes(p.binary)))binary=p.binary;autoConnect=p.autoConnect===true;}catch{}
  if(binary==='codex'&&process.platform==='darwin')for(const p of ['/opt/homebrew/bin/codex','/usr/local/bin/codex',path.join(os.homedir(),'.local/bin/codex')])if(fs.existsSync(p)){binary=p;break;}
  state.binaryLabel=path.basename(binary);
  session.defaultSession.setPermissionRequestHandler((_w,_p,callback)=>callback(false));session.defaultSession.setPermissionCheckHandler(()=>false);
  win=new BrowserWindow({width:490,height:780,minWidth:390,minHeight:540,show:false,title:'Quota Glass',backgroundColor:'#101815',webPreferences:{preload:path.join(__dirname,'preload.cjs'),nodeIntegration:false,contextIsolation:true,sandbox:true,webSecurity:true,devTools:!app.isPackaged}});
  win.webContents.setWindowOpenHandler(()=>({action:'deny'}));win.webContents.on('will-navigate',event=>event.preventDefault());win.on('close',event=>{if(!quitting){event.preventDefault();win.hide();}});
  // Dedicated local-only renderer. Deny remote requests even if content is compromised.
  session.defaultSession.webRequest.onBeforeRequest((details,callback)=>callback({cancel:!details.url.startsWith('file://')}));
  const icon=nativeImage.createFromPath(path.join(__dirname,'../../assets/tray.png'));icon.setTemplateImage(process.platform==='darwin');tray=new Tray(icon);tray.on('click',show);updateTray();
  handle('state',()=>({...state,history:state.status==='demo'?{samples:[],events:[]}:history.snapshot()}));handle('connect',async()=>{await connect();return state;});handle('refresh',refresh);handle('login',login);
  handle('cancel-login',async()=>{const attempt=++loginAttempt,gen=generation,rpc=client,id=loginId;if(rpc&&id)await rpc.request('account/login/cancel',{loginId:id}).catch(()=>{});if(gen!==generation||attempt!==loginAttempt)return state;loginId=null;state.loginPending=false;state.message='已取消登入。';emit();return state;});
  handle('choose-binary',async()=>{const r=await dialog.showOpenDialog(win,{title:'選擇官方 Codex 執行檔',properties:['openFile'],...(process.platform==='win32'?{filters:[{name:'Codex executable',extensions:['exe']}]}:{})});if(!r.canceled&&r.filePaths[0]){stop();binary=r.filePaths[0];state.binaryLabel=path.basename(binary);autoConnect=false;state.status='setup';state.buckets=[];state.lastRefresh=null;state.message='已選執行檔，可連接帳戶。';save();emit();}return state;});
  handle('tag-reset',(id,tag)=>{if(state.status==='demo')return {error:'示範模式不能修改真實歷史。'};if(typeof id!=='string'||!tag||typeof tag!=='object')throw new Error('Invalid history tag');const previous=history.snapshot().events.find(event=>event.id===id);if(!previous||!history.tag(id,{source:tag.source,note:tag.note}))return {error:'無效的歷史標記。'};if(!saveHistory()){history.tag(id,previous);emit();return {error:state.historyError};}emit();return state;});
  handle('export-history',async()=>{if(state.status==='demo')return {saved:false,error:'示範模式不能匯出真實歷史。'};const r=await dialog.showSaveDialog(win,{title:'匯出本機配額歷史',defaultPath:'Quota-Glass-history.csv',filters:[{name:'CSV',extensions:['csv']}]});if(!r.canceled&&r.filePath){try{fs.writeFileSync(r.filePath,'\uFEFF'+history.csv(),{encoding:'utf8',mode:0o600});return {saved:true};}catch{return {saved:false,error:'無法儲存 CSV，請選另一個位置。'};}}return {saved:false};});
  handle('clear-history',async()=>{if(state.status==='demo')return {error:'示範模式不能清除真實歷史。'};const r=await dialog.showMessageBox(win,{type:'warning',buttons:['取消','清除歷史'],defaultId:0,cancelId:0,message:'清除所有本機配額歷史？',detail:'此動作不能復原。帳戶登入和設定不受影響。'});if(r.response===1){const previous=history.snapshot();history.clear();if(!saveHistory()){history=new UsageHistory(previous);await dialog.showMessageBox(win,{type:'error',message:'未能清除磁碟上的歷史。',detail:'已保留現有記錄，請修正儲存權限後再試。'});}emit();}return state;});
  handle('settings',input=>{state.settings=validateSettings(input||{});save();schedule();emit();return state;});
  handle('demo',enabled=>{if(typeof enabled!=='boolean')throw new Error('Invalid mode');stop();alerts.reset();if(enabled){state.status='demo';state.message='僅示範資料，未連接任何帳戶。';state.buckets=demo();state.lastRefresh=Date.now();}else{state.status='setup';state.message='連接後查看帳戶配額。';state.buckets=[];state.lastRefresh=null;}emit();return state;});handle('hide',()=>win.hide());
  win.loadFile(path.join(__dirname,'../renderer/index.html'));win.once('ready-to-show',()=>{show();if(process.argv.includes('--demo')){state.status='demo';state.buckets=demo();state.message='僅示範資料，未連接任何帳戶。';state.lastRefresh=Date.now();emit();}else if(autoConnect)connect();});
  powerMonitor.on('resume',()=>{if(client)refresh();});setInterval(updateTray,15000).unref();
 });
 app.on('window-all-closed',()=>{});app.on('activate',()=>win&&show());
}
