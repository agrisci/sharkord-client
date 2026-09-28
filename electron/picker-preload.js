const { contextBridge, ipcRenderer } = require('electron')

contextBridge.exposeInMainWorld('pickerAPI', {
  onInit:           (cb) => ipcRenderer.on('init', (_, d) => cb(d)),
  listAudio:        ()   => ipcRenderer.invoke('virtmic-list'),
  getAudioSettings: ()   => ipcRenderer.invoke('audio-settings-get'),
  setAudioSettings: (s)  => ipcRenderer.invoke('audio-settings-set', s),
  goLive:           (p)  => ipcRenderer.invoke('picker-go-live', p),
  cancel:           ()   => ipcRenderer.send('picker-cancelled'),
})
