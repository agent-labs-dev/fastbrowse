const { contextBridge, ipcRenderer } = require('electron');
contextBridge.exposeInMainWorld('api', {
  openSettings: () => ipcRenderer.invoke('open-settings'),
  save: (items) => ipcRenderer.invoke('save', items),
});
