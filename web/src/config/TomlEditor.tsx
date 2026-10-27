/**
 * CodeMirror 6 TOML 编辑器（WP-65，UI §8.1 配置 Tab 的高级编辑）。
 *
 * 懒加载 chunk 的默认导出：CodeMirror 及其依赖只进这个 chunk，
 * 主 bundle 不引用（ClusterDetailPage 通过 React.lazy 挂载）。
 *
 * 数据流：
 * - `GET /clusters/{name}/config`：当前修订的 TOML（秘密为哨兵）；
 * - 编辑停顿 500ms → `POST /configs/validate`：诊断列表 + 行内 lint 标记
 *   （labels 的 offset/len 映射回文档位置，越界钳制）；
 * - `PUT /clusters/{name}/config`：base_rev 乐观并发；409 → 冲突提示 + 重新加载。
 */
import { useEffect, useRef, useState } from 'react';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Alert, App, Button, Card, Space, Spin, Tag, Typography } from 'antd';
import { EditorState } from '@codemirror/state';
import { EditorView, keymap, lineNumbers } from '@codemirror/view';
import { StreamLanguage } from '@codemirror/language';
import { setDiagnostics as setLintDiagnostics } from '@codemirror/lint';
import { defaultKeymap, history, historyKeymap } from '@codemirror/commands';
import { toml } from '@codemirror/legacy-modes/mode/toml';
import { ApiError } from '../api/client';
import { fetchClusterConfig, saveClusterConfig } from '../api/config';
import { validateConfig } from '../api/wizard';
import type { Diagnostic } from '../api/types';
import { t } from '../i18n/zh-CN';
import { diagnosticsToEditor, VALIDATE_DEBOUNCE_MS } from './diagnostics';

/** 编辑器基础扩展：行号、历史、TOML 语法、默认键位与外观。 */
function baseExtensions(onDocChange: (text: string) => void) {
  return [
    lineNumbers(),
    history(),
    keymap.of([...defaultKeymap, ...historyKeymap]),
    StreamLanguage.define(toml),
    EditorState.readOnly.of(false),
    EditorView.theme({
      '&': {
        height: '480px',
        fontSize: '13px',
        border: '1px solid #d9d9d9',
        borderRadius: '6px',
        backgroundColor: '#fff',
      },
      '.cm-scroller': {
        overflow: 'auto',
        fontFamily:
          "ui-monospace, SFMono-Regular, 'SF Mono', Menlo, Consolas, 'Liberation Mono', monospace",
      },
      '.cm-gutters': { backgroundColor: '#fafafa' },
    }),
    EditorView.updateListener.of((update) => {
      if (update.docChanged) {
        onDocChange(update.state.doc.toString());
      }
    }),
  ];
}

/** 配置 Tab 的高级 TOML 编辑器（挂载即加载当前修订）。 */
export default function TomlEditor({ cluster }: { cluster: string }) {
  const queryClient = useQueryClient();
  const { message } = App.useApp();

  const configQuery = useQuery({
    queryKey: ['cluster-config', cluster],
    queryFn: ({ signal }) => fetchClusterConfig(cluster, signal),
  });
  const loaded = configQuery.data;

  /** 编辑器正文（null = 修订尚未载入）。 */
  const [text, setText] = useState<string | null>(null);
  /** base_rev：载入（或保存）时的修订号。 */
  const [rev, setRev] = useState(0);
  const [diagnostics, setDiagnostics] = useState<Diagnostic[]>([]);
  const [validating, setValidating] = useState(false);
  const [conflict, setConflict] = useState(false);

  const hostRef = useRef<HTMLDivElement | null>(null);
  const viewRef = useRef<EditorView | null>(null);
  /** 防抖校验的代次：只应用最新一次停顿的结果。 */
  const validateSeq = useRef(0);

  /* ------------------------------ 编辑器创建 ------------------------------ */

  // 修订载入后创建一次；之后的数据刷新不重建（重新加载走 reload）。
  useEffect(() => {
    if (loaded == null || viewRef.current != null || hostRef.current == null) return;
    const view = new EditorView({
      doc: loaded.toml,
      parent: hostRef.current,
      extensions: baseExtensions(setText),
    });
    viewRef.current = view;
    setText(loaded.toml);
    setRev(loaded.rev);
  }, [loaded]);

  // 卸载时销毁视图（独立 effect：数据变化不触发清理）。
  useEffect(
    () => () => {
      viewRef.current?.destroy();
      viewRef.current = null;
    },
    [],
  );

  /* ------------------------------ 防抖校验 ------------------------------ */

  useEffect(() => {
    if (text == null) return;
    const seq = (validateSeq.current += 1);
    setValidating(true);
    const handle = setTimeout(() => {
      void validateConfig(text)
        .then((outcome) => {
          if (validateSeq.current !== seq) return;
          setDiagnostics(outcome.diagnostics);
          setValidating(false);
        })
        .catch(() => {
          if (validateSeq.current !== seq) return;
          setDiagnostics([]);
          setValidating(false);
        });
    }, VALIDATE_DEBOUNCE_MS);
    return () => clearTimeout(handle);
  }, [text]);

  // 诊断 → 行内 lint 标记（setDiagnostics 会按需启用 lint 扩展）。
  useEffect(() => {
    const view = viewRef.current;
    if (!view) return;
    const mapped = diagnosticsToEditor(diagnostics, view.state.doc.length);
    view.dispatch(setLintDiagnostics(view.state, mapped));
  }, [diagnostics]);

  /* -------------------------------- 保存 -------------------------------- */

  const saveMutation = useMutation({
    mutationFn: () => {
      const tomlText = viewRef.current?.state.doc.toString() ?? text ?? '';
      return saveClusterConfig(cluster, { toml: tomlText, baseRev: rev });
    },
    onSuccess: async (saved) => {
      setConflict(false);
      setRev(saved.rev);
      // 缓存同步到刚保存的修订：后续 refetch 不会用旧文档覆盖本地状态。
      queryClient.setQueryData(['cluster-config', cluster], {
        rev: saved.rev,
        toml: viewRef.current?.state.doc.toString() ?? text ?? '',
        sha256: saved.sha256,
      });
      message.success(t('toml.saved', { rev: saved.rev }));
      await queryClient.invalidateQueries({ queryKey: ['cluster', cluster] });
    },
    onError: (error) => {
      setConflict(error instanceof ApiError && error.status === 409);
    },
  });

  /** 放弃本地修改，重新拉取当前修订。 */
  const reload = async () => {
    const fresh = await configQuery.refetch();
    const view = viewRef.current;
    if (!fresh.data || !view) return;
    view.dispatch({
      changes: { from: 0, to: view.state.doc.length, insert: fresh.data.toml },
    });
    setText(fresh.data.toml);
    setRev(fresh.data.rev);
    setConflict(false);
    setDiagnostics([]);
  };

  /* -------------------------------- 渲染 -------------------------------- */

  if (configQuery.isError) {
    return (
      <Card size="small" title={t('clusterDetail.config.editorTitle')}>
        <Alert
          type="error"
          showIcon
          data-testid="toml-load-error"
          message={t('toml.loadFailed', {
            message: configQuery.error instanceof Error
              ? configQuery.error.message
              : String(configQuery.error),
          })}
          action={
            <Button size="small" onClick={() => void configQuery.refetch()}>
              {t('common.retry')}
            </Button>
          }
        />
      </Card>
    );
  }

  const notLoaded = text == null;

  return (
    <Card
      size="small"
      title={t('clusterDetail.config.editorTitle')}
      data-testid="toml-editor-card"
      extra={
        <Space wrap>
          <Tag data-testid="toml-rev">{t('toml.baseRev', { rev })}</Tag>
          <Button
            size="small"
            disabled={notLoaded}
            onClick={() => void reload()}
            data-testid="toml-reload"
          >
            {t('toml.reload')}
          </Button>
          <Button
            type="primary"
            size="small"
            loading={saveMutation.isPending}
            disabled={notLoaded}
            onClick={() => saveMutation.mutate()}
            data-testid="toml-save"
          >
            {t('toml.save')}
          </Button>
        </Space>
      }
    >
      {notLoaded ? (
        <div style={{ padding: '24px 0', textAlign: 'center' }}>
          <Spin />
        </div>
      ) : null}
      {/* 宿主节点始终渲染：编辑器在数据就绪后挂载（不能以 text 为条件）。 */}
      <div ref={hostRef} data-testid="toml-editor" />
      {text == null ? null : (
        <>
          <Space wrap style={{ marginTop: 8, width: '100%' }}>
            {validating ? (
              <Typography.Text type="secondary">{t('toml.validating')}</Typography.Text>
            ) : diagnostics.length === 0 ? (
              <Typography.Text type="success">{t('toml.valid')}</Typography.Text>
            ) : (
              <Typography.Text type="danger">
                {t('toml.invalid', { n: diagnostics.length })}
              </Typography.Text>
            )}
          </Space>
          {diagnostics.length > 0 ? (
            <Alert
              type="error"
              showIcon
              style={{ marginTop: 8 }}
              message={t('toml.diagnostics')}
              description={
                <ul data-testid="toml-diagnostics" style={{ margin: 0, paddingLeft: 18 }}>
                  {diagnostics.map((diagnostic, index) => (
                    <li key={index}>
                      {diagnostic.message}
                      {diagnostic.help ? `（${diagnostic.help}）` : ''}
                    </li>
                  ))}
                </ul>
              }
            />
          ) : null}
          {conflict ? (
            <Alert
              type="warning"
              showIcon
              style={{ marginTop: 8 }}
              data-testid="toml-conflict"
              message={t('toml.conflict')}
              description={t('toml.conflictDescription')}
              action={
                <Button size="small" onClick={() => void reload()}>
                  {t('toml.conflictReload')}
                </Button>
              }
            />
          ) : null}
          {saveMutation.isError && !conflict ? (
            <Alert
              type="error"
              showIcon
              style={{ marginTop: 8 }}
              data-testid="toml-save-error"
              message={t('toml.saveFailed', {
                message: saveMutation.error instanceof Error
                  ? saveMutation.error.message
                  : String(saveMutation.error),
              })}
            />
          ) : null}
        </>
      )}
    </Card>
  );
}
