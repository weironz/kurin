import { beforeEach, describe, expect, it } from 'vitest';
import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { App as AntdApp } from 'antd';
import { http, HttpResponse } from 'msw';
import { server } from '../mocks/server';
import { loginWithToken } from '../api/client';
import type { ClusterConfigView } from '../api/config';
import TomlEditor from './TomlEditor';
import { diagnosticsToEditor, diagnosticToEditor } from './diagnostics';
import { isValidSemver } from '../utils/semver';

/* ------------------------------ 纯函数单测 ------------------------------ */

describe('诊断 → CodeMirror lint 映射（WP-65 F11）', () => {
  it('labels 的 offset/len 映射为 from/to（含 help 无关性）', () => {
    const doc = 'version = "v1alpha2"\nname = "tier1"\n';
    const mapped = diagnosticsToEditor(
      [
        {
          message: '未知字段',
          help: '删除它',
          labels: [
            { offset: 20, len: 6, text: 'tier1' },
            { offset: 0, len: 7, text: 'version' },
          ],
        },
      ],
      doc.length,
    );
    expect(mapped).toEqual([
      { from: 20, to: 26, severity: 'error', message: '未知字段' },
      { from: 0, to: 7, severity: 'error', message: '未知字段' },
    ]);
  });

  it('越界钳制：offset 超出文档长度时贴到末尾；from ≤ to ≤ docLength', () => {
    const doc = 'abc';
    const mapped = diagnosticsToEditor(
      [{ message: 'm', labels: [{ offset: 10, len: 5, text: '…' }] }],
      doc.length,
    );
    expect(mapped).toEqual([{ from: 3, to: 3, severity: 'error', message: 'm' }]);
    expect(mapped[0].from).toBeLessThanOrEqual(mapped[0].to);
    expect(mapped[0].to).toBeLessThanOrEqual(doc.length);
  });

  it('无 labels 的诊断退化为文档开头的 0 长度锚点', () => {
    expect(diagnosticToEditor({ message: '需要 [nodes.*]' }, 10)).toEqual([
      { from: 0, to: 0, severity: 'error', message: '需要 [nodes.*]' },
    ]);
  });
});

describe('isValidSemver', () => {
  it('接受 x.y.z（含 v 前缀与 prerelease），拒绝短版本与自由文本', () => {
    expect(isValidSemver('1.34.0')).toBe(true);
    expect(isValidSemver('  v1.34.0-rc.1 ')).toBe(true);
    expect(isValidSemver('1.34.0+build.2')).toBe(true);
    expect(isValidSemver('1.34')).toBe(false);
    expect(isValidSemver('latest')).toBe(false);
    expect(isValidSemver('')).toBe(false);
  });
});

/* ------------------------------ 组件行为 ------------------------------ */

const VALID_TOML = 'version = "v1alpha2"\nname = "tier1"\n[nodes.cp-1]\naddress = "10.0.0.11"\n';

/** 命中的路由（无导航：编辑器不跳转，探针只防御性兜底）。 */
function renderEditor(name = 'tier1'): void {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0 } },
  });
  render(
    <QueryClientProvider client={queryClient}>
      <AntdApp>
        <MemoryRouter initialEntries={[`/clusters/${name}`]}>
          <Routes>
            <Route path="/clusters/:name" element={<TomlEditor cluster={name} />} />
          </Routes>
        </MemoryRouter>
      </AntdApp>
    </QueryClientProvider>,
  );
}

/** 覆盖 GET /clusters/{name}/config：固定 rev + toml。 */
function givenConfig(view: Partial<ClusterConfigView> & { toml: string }): void {
  server.use(
    http.get('/api/v1/clusters/:name/config', () =>
      HttpResponse.json({ rev: 7, sha256: 'deadbeef', ...view }),
    ),
  );
}

/** 编辑器正文（CodeMirror 的 content 元素）。 */
function editorContent(): HTMLElement {
  const host = screen.getByTestId('toml-editor');
  const content = host.querySelector('.cm-content');
  if (!(content instanceof HTMLElement)) throw new Error('cm-content 未渲染');
  return content;
}

beforeEach(async () => {
  await loginWithToken('valid-token');
});

describe('TOML 编辑器（WP-65）', () => {
  it('加载当前修订：显示基准修订与正文；停顿 500ms 后自动校验', async () => {
    const validations: string[] = [];
    givenConfig({ rev: 7, toml: VALID_TOML });
    server.use(
      http.post('/api/v1/configs/validate', async ({ request }) => {
        const body = (await request.json()) as { toml?: string };
        validations.push(body.toml ?? '');
        return HttpResponse.json({ valid: true });
      }),
    );

    renderEditor();
    await waitFor(() => expect(screen.getByTestId('toml-rev')).toHaveTextContent('#7'));
    // CodeMirror 挂载并载入正文
    await waitFor(() => expect(editorContent().textContent).toContain('v1alpha2'));
    // 载入即校验（停顿 500ms）
    await waitFor(() => expect(validations).toEqual([VALID_TOML]), { timeout: 4_000 });
    await waitFor(() => expect(screen.getByText('校验通过')).toBeInTheDocument());
  });

  it('编辑后防抖校验：诊断列表展示，lint 标记映射到文档区间', async () => {
    givenConfig({ rev: 3, toml: VALID_TOML });
    server.use(
      http.post('/api/v1/configs/validate', async ({ request }) => {
        const body = (await request.json()) as { toml?: string };
        const toml = body.toml ?? '';
        if (toml.includes('!!')) {
          return HttpResponse.json(
            {
              code: 'validation-failed',
              message: '校验失败',
              diagnostics: [
                {
                  message: '语法错误：非法字符',
                  help: '删掉 !!',
                  labels: [{ offset: toml.indexOf('!!'), len: 2, text: '!!' }],
                },
              ],
            },
            { status: 422 },
          );
        }
        return HttpResponse.json({ valid: true });
      }),
    );

    const user = userEvent.setup();
    renderEditor();
    await waitFor(() => expect(editorContent().textContent).toContain('v1alpha2'));

    // 在正文末尾追加 "!!" 触发校验失败
    await user.click(editorContent());
    await user.keyboard('{Control>}{End}{/Control}');
    await user.keyboard('!!');

    // 诊断列表出现（防抖 500ms 后）
    const list = await screen.findByTestId('toml-diagnostics', {}, { timeout: 4_000 });
    expect(within(list).getByText(/语法错误：非法字符/)).toBeInTheDocument();
    expect(screen.getByText(/校验未通过（1 条诊断）/)).toBeInTheDocument();
    // 行内标记：CodeMirror 画了下划线 decoration
    await waitFor(() =>
      expect(
        screen.getByTestId('toml-editor').querySelector('.cm-lintRange'),
      ).not.toBeNull(),
    );
  });

  it('保存成功：PUT 带 base_rev，成功后显示新修订', async () => {
    const puts: Array<Record<string, unknown>> = [];
    givenConfig({ rev: 7, toml: VALID_TOML });
    server.use(
      http.post('/api/v1/configs/validate', () => HttpResponse.json({ valid: true })),
      http.put('/api/v1/clusters/:name/config', async ({ request }) => {
        puts.push((await request.json()) as Record<string, unknown>);
        return HttpResponse.json({ rev: 8, sha256: 'feedface' });
      }),
    );

    const user = userEvent.setup();
    renderEditor();
    await waitFor(() => expect(editorContent().textContent).toContain('v1alpha2'));
    await user.click(screen.getByTestId('toml-save'));

    await waitFor(() => expect(puts).toHaveLength(1));
    expect(puts[0]).toMatchObject({ toml: VALID_TOML, base_rev: 7 });
    await waitFor(() => expect(screen.getByTestId('toml-rev')).toHaveTextContent('#8'));
  });

  it('保存冲突（409）：显示冲突提示，重新加载拉回服务端修订', async () => {
    let serverRev = 7;
    const serverToml = 'version = "v1alpha2"\nname = "tier1"\n';
    const puts: Array<Record<string, unknown>> = [];
    server.use(
      http.get('/api/v1/clusters/:name/config', () =>
        HttpResponse.json({ rev: serverRev, toml: serverToml, sha256: 'deadbeef' }),
      ),
      http.post('/api/v1/configs/validate', () => HttpResponse.json({ valid: true })),
      http.put('/api/v1/clusters/:name/config', async ({ request }) => {
        puts.push((await request.json()) as Record<string, unknown>);
        // 服务端已被别人推进到 rev 9：base_rev=7 过期 → 409
        return HttpResponse.json(
          { code: 'conflict', message: '配置已被修改（当前修订 9）' },
          { status: 409 },
        );
      }),
    );

    const user = userEvent.setup();
    renderEditor();
    await waitFor(() => expect(editorContent().textContent).toContain('tier1'));

    await user.click(screen.getByTestId('toml-save'));
    expect(await screen.findByTestId('toml-conflict')).toHaveTextContent(/修订冲突/);
    expect(puts).toHaveLength(1);
    expect(puts[0]).toMatchObject({ base_rev: 7 });

    // 服务端推进到 rev 9；重新加载 → 拉回新修订与新正文
    serverRev = 9;
    await user.click(screen.getByTestId('toml-reload'));
    await waitFor(() => expect(screen.getByTestId('toml-rev')).toHaveTextContent('#9'));
    await waitFor(() =>
      expect(screen.queryByTestId('toml-conflict')).not.toBeInTheDocument(),
    );
  });

  it('配置加载失败（409 无配置）：显示错误与重试', async () => {
    server.use(
      http.get('/api/v1/clusters/:name/config', () =>
        HttpResponse.json(
          { code: 'conflict', message: '集群 tier1 还没有配置' },
          { status: 409 },
        ),
      ),
    );
    renderEditor();
    expect(await screen.findByTestId('toml-load-error')).toHaveTextContent(
      /集群 tier1 还没有配置/,
    );
  });
});
