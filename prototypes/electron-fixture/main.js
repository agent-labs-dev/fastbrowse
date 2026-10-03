const { app, BrowserWindow, ipcMain, dialog } = require('electron');
const path = require('path');
let win;
app.whenReady().then(() => {
  win = new BrowserWindow({ width: 1000, height: 700, webPreferences: { preload: path.join(__dirname, 'preload.js') } });
  win.loadFile('index.html');
  ipcMain.handle('open-settings', () => {
    const s = new BrowserWindow({ width: 600, height: 400, parent: win });
    s.loadFile('settings.html');
  });
  ipcMain.handle('save', (_e, items) => { console.log('SAVED', JSON.stringify(items)); return items.length; });
});
app.on('window-all-closed', () => app.quit());
