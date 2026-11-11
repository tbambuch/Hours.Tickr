'use strict';

// Renderer-side i18n: the dictionary of the active language comes from the main process over IPC.
// Static texts use data-i18n / data-i18n-title / data-i18n-aria-label / data-i18n-placeholder attributes;
// <html data-title-key="..."> sets the document title.
(function () {
  let dict = {};
  let lang = 'en';
  const listeners = [];

  function t(key, params) {
    const raw = dict[key] !== undefined ? dict[key] : key;
    return params ? raw.replace(/\{(\w+)\}/g, (m, k) => (params[k] !== undefined ? String(params[k]) : m)) : raw;
  }

  function apply() {
    document.documentElement.lang = lang;
    const titleKey = document.documentElement.dataset.titleKey;
    if (titleKey) document.title = t(titleKey);
    for (const [attr, target] of [['data-i18n', null], ['data-i18n-title', 'title'], ['data-i18n-aria-label', 'aria-label'], ['data-i18n-placeholder', 'placeholder']]) {
      for (const el of document.querySelectorAll('[' + attr + ']')) {
        const value = t(el.getAttribute(attr));
        if (target) el.setAttribute(target, value);
        else el.textContent = value;
      }
    }
  }

  function set(payload) {
    lang = payload.lang;
    dict = payload.dict;
    apply();
    for (const fn of listeners) fn(lang);
  }

  window.i18n = {
    t,
    apply,
    get lang() {
      return lang;
    },
    /** Called (after the DOM was re-translated) whenever the language changes. */
    onChange: (fn) => listeners.push(fn),
    ready: window.timewatch.getI18n().then((p) => {
      set(p);
      window.timewatch.onI18n(set);
    }),
  };
})();
