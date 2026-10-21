'use strict';
const {contextBridge,ipcRenderer}=require('electron');
const invoke=channel=>(...args)=>ipcRenderer.invoke(channel,...args);
contextBridge.exposeInMainWorld('quota',Object.freeze({getState:invoke('state'),tagReset:invoke('tag-reset'),exportHistory:invoke('export-history'),clearHistory:invoke('clear-history'),refresh:invoke('refresh'),connect:invoke('connect'),login:invoke('login'),cancelLogin:invoke('cancel-login'),chooseBinary:invoke('choose-binary'),setSettings:invoke('settings'),setDemo:invoke('demo'),hide:invoke('hide'),onState(callback){const listener=(_event,value)=>callback(value);ipcRenderer.on('state-update',listener);return ()=>ipcRenderer.removeListener('state-update',listener);}}));
