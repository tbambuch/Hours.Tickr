'use strict';

const $ = (id) => document.getElementById(id);
const api = window.hoursTickr;
let loaded = null;
const t = (k, p) => window.i18n.t(k, p);

function setStatus(el, text, kind) {
  el.textContent = text || '';
  el.className = 'status' + (kind ? ' ' + kind : '');
}

function fillProjects(projects, selectedId) {
  const sel = $('project');
  sel.textContent = '';
  const empty = document.createElement('option');
  empty.value = '';
  empty.dataset.i18n = 'settings.selectProject';
  empty.textContent = t('settings.selectProject');
  sel.appendChild(empty);
  for (const p of projects) {
    const o = document.createElement('option');
    o.value = String(p.id);
    o.textContent = p.name;
    sel.appendChild(o);
  }
  if (selectedId !== null && selectedId !== undefined && projects.some((p) => String(p.id) === String(selectedId))) {
    sel.value = String(selectedId);
  }
  sel.disabled = false;
}

async function loadTasks(projectId, selectedTaskId) {
  const sel = $('task');
  sel.textContent = '';
  const none = document.createElement('option');
  none.value = '';
  none.dataset.i18n = 'settings.noTask';
  none.textContent = t('settings.noTask');
  sel.appendChild(none);
  sel.disabled = true;
  if (!projectId) return;
  const res = await api.getTasks(projectId);
  if (!res.ok) {
    setStatus($('saveStatus'), t('settings.tasksFailed', { error: res.error }), 'err');
    sel.disabled = false;
    return;
  }
  for (const task of res.tasks) {
    const o = document.createElement('option');
    o.value = String(task.id);
    o.textContent = task.name;
    sel.appendChild(o);
  }
  if (selectedTaskId !== null && selectedTaskId !== undefined && res.tasks.some((task) => String(task.id) === String(selectedTaskId))) {
    sel.value = String(selectedTaskId);
  }
  sel.disabled = false;
}

async function loadProjects(selectedId) {
  const res = await api.getProjects();
  if (!res.ok) {
    setStatus($('keyStatus'), res.error, 'err');
    return false;
  }
  fillProjects(res.projects, selectedId);
  return true;
}

async function init() {
  await window.i18n.ready;
  loaded = await api.getSettings();
  $('language').value = loaded.language;
  $('theme').value = loaded.theme;
  $('note').value = loaded.note || '';
  $('interval').value = loaded.syncIntervalMin;
  $('idle').value = loaded.idleMinutes;
  $('autostart').checked = loaded.openAtLogin;
  if (!loaded.secureStorage) {
    setStatus($('keyStatus'), t('err.noStorageKey'), 'err');
  }
  renderAccount();
  if (loaded.hasKey) {
    $('apiKey').placeholder = t('settings.keyStored', { mask: loaded.keyMask });
    $('keySection').open = true;
  }
  if (loaded.authMethod) {
    if (await loadProjects(loaded.projectId)) {
      await loadTasks(loaded.projectId, loaded.taskId);
    }
  }
}

function renderAccount() {
  const oauth = loaded.authMethod === 'oauth';
  const who = loaded.accountName || loaded.accountEmail;
  const text = oauth
    ? t('settings.loggedInAs', {
        who: who ? loaded.accountName + (loaded.accountEmail ? ' (' + loaded.accountEmail + ')' : '') : t('settings.loggedInFallback'),
      })
    : loaded.authMethod === 'apikey'
      ? t('settings.usingKey', { mask: loaded.keyMask })
      : t('settings.notLoggedIn');
  $('accountInfo').textContent = text;
  $('login').hidden = oauth;
  $('logout').hidden = !oauth;
  if (oauth && loaded.authError) setStatus($('loginStatus'), t('err.loginExpired'), 'err');
  if (oauth && loaded.authError) $('login').hidden = false;
}

async function afterAuthChange(message) {
  loaded = await api.getSettings();
  renderAccount();
  if (message) setStatus($('loginStatus'), message, 'ok');
  $('project').textContent = '';
  $('project').disabled = true;
  $('task').disabled = true;
  if (loaded.authMethod && (await loadProjects(loaded.projectId))) await loadTasks(loaded.projectId, loaded.taskId);
}

$('login').addEventListener('click', async () => {
  const btn = $('login');
  btn.disabled = true;
  setStatus($('loginStatus'), t('settings.loginWaiting'));
  try {
    const res = await api.login();
    if (res.ok) await afterAuthChange(t('settings.loginOk'));
    else setStatus($('loginStatus'), res.cancelled ? '' : res.error, res.cancelled ? '' : 'err');
  } finally {
    btn.disabled = false;
  }
});

$('logout').addEventListener('click', async () => {
  await api.logout();
  setStatus($('loginStatus'), t('settings.loggedOut'), 'ok');
  await afterAuthChange();
  fillProjects([], null);
  $('project').disabled = true;
});

$('verify').addEventListener('click', async () => {
  const btn = $('verify');
  btn.disabled = true;
  setStatus($('keyStatus'), t('settings.verifying'));
  try {
    const res = await api.verifyKey($('apiKey').value.trim());
    if (!res.ok) {
      setStatus($('keyStatus'), res.error, 'err');
      return;
    }
    $('apiKey').value = '';
    $('apiKey').placeholder = t('settings.keyStored', { mask: res.keyMask });
    setStatus($('keyStatus'), t('settings.keyValid', { n: res.projects.length }), 'ok');
    loaded = await api.getSettings();
    renderAccount();
    fillProjects(res.projects, $('project').value || (loaded && loaded.projectId));
    await loadTasks($('project').value, loaded && loaded.taskId);
  } finally {
    btn.disabled = false;
  }
});

$('project').addEventListener('change', () => loadTasks($('project').value, null));

$('save').addEventListener('click', async () => {
  const sel = $('project');
  const projectId = sel.value || null;
  const taskSel = $('task');
  const res = await api.saveSettings({
    projectId,
    projectName: projectId ? sel.options[sel.selectedIndex].textContent : '',
    taskId: taskSel.value || null,
    taskName: taskSel.value ? taskSel.options[taskSel.selectedIndex].textContent : '',
    note: $('note').value,
    syncIntervalMin: Number($('interval').value),
    idleMinutes: Number($('idle').value),
    openAtLogin: $('autostart').checked,
    language: $('language').value,
    theme: $('theme').value,
  });
  if (res.ok) {
    setStatus($('saveStatus'), t('settings.saved'), 'ok');
    loaded = await api.getSettings();
  } else {
    setStatus($('saveStatus'), res.error, 'err');
  }
});

// Language and appearance apply immediately (the other fields still need "Save").
for (const id of ['language', 'theme']) {
  $(id).addEventListener('change', () => api.setUi({ language: $('language').value, theme: $('theme').value }));
}

window.i18n.onChange(() => {
  // static texts were already re-translated; refresh the dynamic ones
  if (!loaded) return;
  renderAccount();
  if (loaded.hasKey) $('apiKey').placeholder = t('settings.keyStored', { mask: loaded.keyMask });
  reportHeight();
});

$('cancel').addEventListener('click', () => api.closeSettings());
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') api.closeSettings();
});

let lastHeight = 0;
function reportHeight() {
  const h = Math.ceil(document.body.offsetHeight);
  if (h !== lastHeight) {
    lastHeight = h;
    api.resizeSettings(h);
  }
}
new ResizeObserver(reportHeight).observe(document.body);

init().then(reportHeight);
