'use strict';
const { contextBridge, ipcRenderer } = require('electron');
contextBridge.exposeInMainWorld('sparkDesktop', Object.freeze({
  updateStatus: () => ipcRenderer.invoke('spark:update-status'),
  checkUpdates: () => ipcRenderer.invoke('spark:check-updates'),
  installUpdate: activity => ipcRenderer.invoke('spark:install-update', activity),
  setActivity: activity => ipcRenderer.invoke('spark:activity', activity),
  ready: () => ipcRenderer.invoke('spark:ready'),
  getSettings: () => ipcRenderer.invoke('spark:get-settings'),
  setSettings: value => ipcRenderer.invoke('spark:set-settings', value),
  quit: () => ipcRenderer.invoke('spark:quit'),
  prepareShareAudio: () => ipcRenderer.invoke('spark:prepare-share-audio'),
  stopShareAudio: () => ipcRenderer.invoke('spark:stop-share-audio'),
  sources: () => ipcRenderer.invoke('spark:sources'),
  selectSource: id => ipcRenderer.invoke('spark:select-source', id),
  saveFile: (id, name) => ipcRenderer.invoke('spark:save-file', { id, name }),
  version: () => ipcRenderer.invoke('spark:version'),
}));
