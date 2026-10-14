// Языки интерфейса. Английский — базовый: чего нет в другом языке, берётся из него.
// tr('ключ', { n: 5 }) — строка с подстановкой {n}; trn('ключ', n) — с формой числа
// (в словаре массив форм: en — [один, много], ru — [один, два–четыре, пять]).
// Разметка: data-i18n="ключ" (текст), data-i18n-title, data-i18n-placeholder, data-i18n-aria.

const I18N = {
  en: {
    'menu.title': 'Menu',
    'menu.settings': 'Settings',
    'menu.about': 'About',
    'settings.title': 'Settings',
    'settings.language': 'Language',
    'settings.theme': 'Theme',
    'settings.theme.system': 'As in the system',
    'settings.theme.light': 'Light',
    'settings.theme.dark': 'Dark',
    'settings.close': 'Close',
  },
  ru: {
    'menu.title': 'Меню',
    'menu.settings': 'Настройки',
    'menu.about': 'О программе',
    'settings.title': 'Настройки',
    'settings.language': 'Язык',
    'settings.theme': 'Тема',
    'settings.theme.system': 'Как в системе',
    'settings.theme.light': 'Светлая',
    'settings.theme.dark': 'Тёмная',
    'settings.close': 'Закрыть',
  },
};

const LANGS = [
  { id: 'en', name: 'English' },
  { id: 'ru', name: 'Русский' },
];

let LANG = (() => {
  try {
    const saved = localStorage.getItem('filecat.lang');
    if (saved && I18N[saved]) return saved;
  } catch { /* недоступно */ }
  // по умолчанию — язык системы, если он есть, иначе английский
  const sys = (navigator.language || 'en').slice(0, 2).toLowerCase();
  return I18N[sys] ? sys : 'en';
})();

function tr(key, vars) {
  let s = I18N[LANG]?.[key] ?? I18N.en[key] ?? key;
  if (Array.isArray(s)) s = s[s.length - 1];
  return vars ? String(s).replace(/\{(\w+)\}/g, (m, k) => (vars[k] ?? m)) : s;
}

// форма слова по числу: «1 файл», «2 файла», «5 файлов» / «1 file», «2 files»
function trn(key, n, vars) {
  const forms = I18N[LANG]?.[key] ?? I18N.en[key];
  if (!Array.isArray(forms)) return tr(key, { n, ...vars });
  let i;
  if (LANG === 'ru') {
    const a = Math.abs(n) % 100, b = a % 10;
    i = a > 10 && a < 20 ? 2 : b === 1 ? 0 : b >= 2 && b <= 4 ? 1 : 2;
  } else {
    i = Math.abs(n) === 1 ? 0 : 1;
  }
  const s = forms[Math.min(i, forms.length - 1)];
  return s.replace(/\{(\w+)\}/g, (m, k) => (k === 'n' ? (typeof fmtNum === 'function' ? fmtNum(n) : n) : vars?.[k] ?? m));
}

function applyI18n(root = document) {
  root.querySelectorAll('[data-i18n]').forEach((el) => { el.textContent = tr(el.dataset.i18n); });
  root.querySelectorAll('[data-i18n-title]').forEach((el) => { el.title = tr(el.dataset.i18nTitle); });
  root.querySelectorAll('[data-i18n-placeholder]').forEach((el) => { el.placeholder = tr(el.dataset.i18nPlaceholder); });
  root.querySelectorAll('[data-i18n-aria]').forEach((el) => { el.setAttribute('aria-label', tr(el.dataset.i18nAria)); });
  document.documentElement.lang = LANG;
}

function setLang(id) {
  if (!I18N[id]) return;
  LANG = id;
  try { localStorage.setItem('filecat.lang', id); } catch { /* недоступно */ }
  applyI18n();
  window.dispatchEvent(new Event('langchange'));
}

// тема: system | light | dark
let THEME = (() => { try { return localStorage.getItem('filecat.theme') || 'system'; } catch { return 'system'; } })();
function applyTheme() {
  if (THEME === 'system') document.documentElement.removeAttribute('data-theme');
  else document.documentElement.dataset.theme = THEME;
}
function setTheme(id) {
  THEME = id;
  try { localStorage.setItem('filecat.theme', id); } catch { /* недоступно */ }
  applyTheme();
}
applyTheme();
