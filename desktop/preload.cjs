'use strict';
const { contextBridge, ipcRenderer } = require('electron');
contextBridge.exposeInMainWorld('sparkDesktop', Object.freeze({
  getSettings: () => ipcRenderer.invoke('spark:get-settings'),
  setSettings: value => ipcRenderer.invoke('spark:set-settings', value),
  quit: () => ipcRenderer.invoke('spark:quit'),
  sources: () => ipcRenderer.invoke('spark:sources'),
  selectSource: id => ipcRenderer.invoke('spark:select-source', id),
  saveFile: (id, name) => ipcRenderer.invoke('spark:save-file', { id, name }),
  version: () => ipcRenderer.invoke('spark:version'),
}));
