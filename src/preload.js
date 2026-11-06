"use strict";

const { contextBridge, ipcRenderer } = require("electron");

contextBridge.exposeInMainWorld("cacaduu", {
  state: () => ipcRenderer.invoke("state"),
  signIn: (email, password) => ipcRenderer.invoke("sign-in", { email, password }),
  signOut: () => ipcRenderer.invoke("sign-out"),
  chooseShop: (id) => ipcRenderer.invoke("choose-shop", id),
  chooseFolder: () => ipcRenderer.invoke("choose-folder"),
  checkNow: () => ipcRenderer.invoke("check-now"),
  setAutostart: (enabled) => ipcRenderer.invoke("set-autostart", enabled),
  openSite: () => ipcRenderer.invoke("open-site"),
  onState: (listener) => ipcRenderer.on("state", (_event, value) => listener(value)),
});
