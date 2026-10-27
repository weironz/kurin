import { describe, expect, it } from 'vitest';
import { zhCN } from './zh-CN';
import { en } from './en';
import { getLocale, setLocale, t } from './index';
import type { MessageKey } from './index';

/** 提取 `{var}` 占位符（排序后比较：翻译允许调整语序，但不允许丢占位符）。 */
function placeholders(text: string): string[] {
  return [...text.matchAll(/\{(\w+)\}/g)].map((m) => m[1]).sort();
}

describe('i18n 字典（WP-78）', () => {
  it('en 与 zh-CN 键集完全一致（编译期已强制，这里是运行时复核）', () => {
    expect(Object.keys(en).sort()).toEqual(Object.keys(zhCN).sort());
  });

  it('每个键的占位符在两个字典中一致', () => {
    for (const [key, zhText] of Object.entries(zhCN)) {
      expect(placeholders(en[key as MessageKey]), key).toEqual(placeholders(zhText));
    }
  });

  it('默认 locale 是 zh-CN', () => {
    expect(getLocale()).toBe('zh-CN');
    expect(window.localStorage.getItem('kurin.locale')).toBeNull();
  });

  it('t 查 zh 表并插值 {var}', () => {
    expect(t('clusters.certDays', { n: 30 })).toBe('30 天');
    expect(t('addons.card.upgradeFrom', { from: '1.2.0' })).toBe('已装 1.2.0，可升级');
  });

  it('切到 en 后 t 返回英文并插值，切回恢复中文', () => {
    setLocale('en');
    try {
      expect(t('nav.clusters')).toBe('Clusters');
      expect(
        t('operationDetail.grid.cellTitle', {
          stage: 'prepare',
          node: 'cp-1',
          n: 3,
          state: 'running',
        }),
      ).toBe('prepare @ cp-1: 3 tasks (running)');
    } finally {
      setLocale('zh-CN');
    }
    expect(t('nav.clusters')).toBe('集群');
  });

  it('en 运行时缺项回退 zh 文案', () => {
    const removed = en['nav.about'];
    delete (en as Record<string, string>)['nav.about'];
    try {
      setLocale('en');
      expect(t('nav.about')).toBe('关于');
    } finally {
      en['nav.about'] = removed;
      setLocale('zh-CN');
    }
  });

  it('两个字典都缺的键回退键名', () => {
    expect(t('no.such.key' as MessageKey)).toBe('no.such.key');
  });

  it('setLocale 持久化到 localStorage（kurin.locale）', () => {
    setLocale('en');
    expect(window.localStorage.getItem('kurin.locale')).toBe('en');
    setLocale('zh-CN');
    expect(window.localStorage.getItem('kurin.locale')).toBe('zh-CN');
  });
});
