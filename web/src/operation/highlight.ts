/**
 * 搜索高亮（WP-63，UI §8.5）：把命中切分为 React 可渲染的片段。
 * 大小写不敏感；空查询原样返回。避免正则转义问题用 indexOf 扫描。
 */
export interface HighlightPart {
  text: string;
  hit: boolean;
}

/** 把 `text` 按 `query`（大小写不敏感）切分为片段。 */
export function splitHighlight(text: string, query: string): HighlightPart[] {
  const parts: HighlightPart[] = [];
  if (!query) {
    if (text) parts.push({ text, hit: false });
    return parts;
  }
  const haystack = text.toLowerCase();
  const needle = query.toLowerCase();
  if (needle.length === 0) {
    parts.push({ text, hit: false });
    return parts;
  }
  let cursor = 0;
  for (;;) {
    const found = haystack.indexOf(needle, cursor);
    if (found < 0) break;
    if (found > cursor) parts.push({ text: text.slice(cursor, found), hit: false });
    parts.push({ text: text.slice(found, found + needle.length), hit: true });
    cursor = found + needle.length;
  }
  if (cursor < text.length) parts.push({ text: text.slice(cursor), hit: false });
  return parts;
}
