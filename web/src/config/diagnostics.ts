/**
 * 服务端诊断 → CodeMirror lint 标记的映射（WP-65，F11）。
 *
 * `POST /configs/validate` 的 422 诊断带 `labels`（文档偏移 + 长度），
 * 编辑器把它们映射成 `@codemirror/lint` 的 Diagnostic 画行内下划线。
 * 纯函数模块：不依赖 CodeMirror 的运行时，可在 jsdom 单测里直接断言。
 */
import type { Diagnostic } from '../api/types';

/** 面向 CodeMirror 的 lint 项（与 @codemirror/lint 的 Diagnostic 同构）。 */
export interface EditorDiagnostic {
  from: number;
  to: number;
  severity: 'error' | 'warning' | 'info';
  message: string;
}

/** 一条服务端诊断展开成行内 lint 项（无 labels 时退化为 0 长度锚点）。 */
export function diagnosticToEditor(diagnostic: Diagnostic, docLength: number): EditorDiagnostic[] {
  const labels = (diagnostic.labels ?? []).filter(
    (label) => Number.isInteger(label.offset) && label.offset >= 0 && label.len >= 0,
  );
  if (labels.length === 0) {
    return [{ from: 0, to: 0, severity: 'error', message: diagnostic.message }];
  }
  return labels.map((label) => {
    const from = Math.min(label.offset, docLength);
    const to = Math.min(label.offset + label.len, docLength);
    return { from, to: Math.max(from, to), severity: 'error' as const, message: diagnostic.message };
  });
}

/** 全量映射：诊断列表 → lint 项（含越界钳制；from ≤ to ≤ docLength）。 */
export function diagnosticsToEditor(
  diagnostics: Diagnostic[],
  docLength: number,
): EditorDiagnostic[] {
  return diagnostics.flatMap((diagnostic) => diagnosticToEditor(diagnostic, docLength));
}

/** 校验防抖间隔（ms）：编辑停顿后触发一次服务端全量校验。 */
export const VALIDATE_DEBOUNCE_MS = 500;
