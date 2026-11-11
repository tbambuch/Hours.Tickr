'use strict';

const $ = (id) => document.getElementById(id);
const recordBtn = $('record');
const pinBtn = $('pin');
let state = null;

function formatHM(ms) {
  const m = Math.floor(Math.max(0, ms) / 60000);
  return `${Math.floor(m / 60)}:${String(m % 60).padStart(2, '0')}`;
}

const t = (k, p) => window.i18n.t(k, p);

function syncText(sync) {
  switch (sync.state) {
    case 'ok': return t('timer.sync.ok');
    case 'pending': return t('timer.sync.pending', { n: sync.pending });
    case 'offline': return t('timer.sync.offline', { n: sync.pending });
    case 'auth-error': return state && state.authMethod === 'oauth' ? t('timer.sync.authOauth') : t('timer.sync.authKey');
    case 'error': return t('timer.sync.error') + (sync.detail ? ': ' + sync.detail : '');
    default: return '';
  }
}

function render(s) {
  state = s;
  recordBtn.dataset.status = s.status;
  recordBtn.setAttribute('aria-label', s.status === 'stopped' ? t('timer.startLabel') : t('timer.stopLabel'));
  $('paused').hidden = s.status !== 'paused';
  $('paused').textContent = s.pauseReason === 'idle' ? t('timer.pausedIdle') : t('timer.paused');

  $('prompt').hidden = s.configured;
  $('time').hidden = !s.configured;
  $('time').textContent = formatHM(s.todayMs);

  pinBtn.setAttribute('aria-pressed', String(s.pinned));
  pinBtn.title = s.pinned ? t('timer.pinOn') : t('timer.pinOff');

  const dot = $('syncDot');
  dot.className = 'dot ' + s.sync.state;
  const text = syncText(s.sync);
  dot.title = text;
  dot.setAttribute('aria-label', text);

  $('project').textContent = s.projectName || '';
  $('project').title = s.projectName || '';
}

recordBtn.addEventListener('click', () => window.timewatch.toggle());
pinBtn.addEventListener('click', () => window.timewatch.setPinned(!(state && state.pinned)));
$('settings').addEventListener('click', () => window.timewatch.openSettings());
$('hide').addEventListener('click', () => window.timewatch.hideWindow());

// Space / Enter = Start/Stop (native buttons keep their own activation behaviour).
document.addEventListener('keydown', (e) => {
  if ((e.code === 'Space' || e.code === 'Enter') && !(e.target instanceof HTMLButtonElement)) {
    e.preventDefault();
    window.timewatch.toggle();
  }
});

window.i18n.onChange(() => state && render(state)); // runs after the static texts were re-translated
window.timewatch.onState(render);
window.i18n.ready.then(() => window.timewatch.getState()).then(render);
recordBtn.focus();
window.addEventListener('focus', () => recordBtn.focus());
