'use strict';

// Exposes `window.timewatch` to the windows on top of Tauri (commands = `invoke`, pushes from Rust = events). Loaded before i18n-client.js in every window.
(function () {
  const tauri = window.__TAURI__;
  const invoke = (cmd, args) => tauri.core.invoke(cmd, args);

  // Subscribes to an event emitted by the Rust side; returns an unsubscribe function.
  function on(name, cb) {
    let unlisten = null;
    let cancelled = false;
    tauri.event.listen(name, (e) => cb(e.payload)).then((fn) => {
      if (cancelled) fn();
      else unlisten = fn;
    });
    return () => {
      cancelled = true;
      if (unlisten) unlisten();
    };
  }

  window.timewatch = {
    // i18n (both windows)
    getI18n: () => invoke('i18n_get'),
    onI18n: (cb) => on('i18n', cb),

    // timer window
    getState: () => invoke('timer_get_state'),
    toggle: () => invoke('timer_toggle'),
    setPinned: (pinned) => invoke('timer_set_pinned', { pinned: !!pinned }),
    openSettings: () => invoke('timer_open_settings'),
    hideWindow: () => invoke('timer_hide'),
    onState: (cb) => on('state', cb),

    // settings window
    getSettings: () => invoke('settings_get'),
    verifyKey: (candidate) => invoke('settings_verify_key', { candidate: typeof candidate === 'string' ? candidate : '' }),
    setUi: (ui) => invoke('settings_set_ui', { ui }),
    login: () => invoke('settings_login'),
    logout: () => invoke('settings_logout'),
    clearKey: () => invoke('settings_clear_key'),
    getProjects: () => invoke('settings_projects'),
    getTasks: (projectId) => invoke('settings_tasks', { projectId }),
    saveSettings: (data) => invoke('settings_save', { data }),
    resizeSettings: (h) => invoke('settings_resize', { height: Math.ceil(Number(h)) }),
    closeSettings: () => invoke('settings_close'),
  };

  // The frameless timer window is dragged by its background.
  document.addEventListener('DOMContentLoaded', () => {
    const card = document.getElementById('card');
    if (!card || !card.classList.contains('drag')) return;
    card.addEventListener('mousedown', (e) => {
      if (e.button !== 0 || e.target.closest('button, input, select, textarea, a, [tabindex]')) return;
      tauri.window.getCurrentWindow().startDragging();
    });
  });
})();
