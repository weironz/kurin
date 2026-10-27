/**
 * i18n 入口（WP-78）：`t()` 查表 + `{var}` 插值，zh-CN / en 语言切换。
 *
 * 词典是纯数据模块（./zh-CN.ts、./en.ts，键集由 TypeScript 强制一致）；
 * 活跃 locale 存在模块级状态里 —— `useLocale()` 通过 useSyncExternalStore 订阅，
 * App / AppShell 持有它，切换时整棵树重渲染，页面里的 `t()` 自然取到新语言。
 * 选择持久化在 localStorage（`kurin.locale`），默认 zh-CN；
 * dayjs 全局 locale 同步切换（antd 的 locale 由 App.tsx 按 useLocale() 映射）。
 */
import { useSyncExternalStore } from 'react';
import dayjs from 'dayjs';
import 'dayjs/locale/zh-cn';
import 'dayjs/locale/en';
import { zhCN } from './zh-CN';
import type { Dictionary, MessageKey } from './zh-CN';
import { en } from './en';

export type { Dictionary, MessageKey };

export type Locale = 'zh-CN' | 'en';

const LOCALE_STORAGE_KEY = 'kurin.locale';
const DAYJS_LOCALE: Record<Locale, string> = { 'zh-CN': 'zh-cn', en: 'en' };
const dictionaries: Record<Locale, Dictionary> = { 'zh-CN': zhCN, en };

function isLocale(value: string | null): value is Locale {
  return value === 'zh-CN' || value === 'en';
}

function readStoredLocale(): Locale {
  try {
    const stored = window.localStorage.getItem(LOCALE_STORAGE_KEY);
    return isLocale(stored) ? stored : 'zh-CN';
  } catch {
    // localStorage 不可用（隐私模式等）时退回默认
    return 'zh-CN';
  }
}

function persistLocale(locale: Locale): void {
  try {
    window.localStorage.setItem(LOCALE_STORAGE_KEY, locale);
  } catch {
    // 持久化失败不影响本次会话内的切换
  }
}

let currentLocale = readStoredLocale();

// 模块加载即对齐 dayjs 全局 locale（默认 en，持久化过 zh-CN 时切回中文）
dayjs.locale(DAYJS_LOCALE[currentLocale]);

const listeners = new Set<() => void>();

/** 当前 locale（React 之外也可用，如 utils/operation.ts 的纯函数）。 */
export function getLocale(): Locale {
  return currentLocale;
}

/** 切换 locale：更新模块状态与 dayjs 全局 locale，持久化并通知订阅者。 */
export function setLocale(locale: Locale): void {
  if (locale === currentLocale) return;
  currentLocale = locale;
  dayjs.locale(DAYJS_LOCALE[locale]);
  persistLocale(locale);
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** 订阅 locale 变化（useSyncExternalStore）：持有它的组件在切换时整棵重渲染。 */
export function useLocale(): Locale {
  return useSyncExternalStore(subscribe, getLocale, getLocale);
}

/** 查表 + `{var}` 插值；en 缺项回退 zh，再缺回退键名。 */
export function t(key: MessageKey, vars?: Record<string, string | number>): string {
  let text: string = dictionaries[currentLocale][key] ?? zhCN[key] ?? key;
  if (vars) {
    for (const [k, v] of Object.entries(vars)) {
      text = text.replaceAll(`{${k}}`, String(v));
    }
  }
  return text;
}
