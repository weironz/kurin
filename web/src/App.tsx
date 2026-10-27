import { lazy, Suspense, type ComponentType, type LazyExoticComponent } from 'react';
import { Routes, Route, Navigate } from 'react-router-dom';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { App as AntdApp, ConfigProvider, Spin } from 'antd';
import zhCN from 'antd/locale/zh_CN';
import dayjs from 'dayjs';
import 'dayjs/locale/zh-cn';
import { establishSession } from './api/client';
import { LoginPage } from './pages/LoginPage';
import { AppShell } from './components/AppShell';

dayjs.locale('zh-cn');

/**
 * 会话状态：`establishSession` 完成 hash 令牌换取（UI §6.10）。
 * 401 → null（未登录）；网络错误 → undefined（isError）。
 */
function useSession() {
  return useQuery({
    queryKey: ['session'],
    queryFn: () => establishSession(),
    retry: 1,
    staleTime: Infinity,
  });
}

/* -------------- 路由懒加载（UI §8.4；code-splitting 必须动态导入） -------------- */

// 动态导入：路由级代码分割由打包器按 chunk 拆分，无法静态导入。
const ClustersPage = lazy(() => import('./pages/ClustersPage'));
const ClusterDetailPage = lazy(() => import('./pages/ClusterDetailPage'));
const OperationsPage = lazy(() => import('./pages/OperationsPage'));
const PackagesPage = lazy(() => import('./pages/PackagesPage'));
const AboutPage = lazy(() => import('./pages/AboutPage'));

function PageFallback() {
  return (
    <div style={{ display: 'grid', placeItems: 'center', minHeight: '50vh' }}>
      <Spin />
    </div>
  );
}

function lazily(Component: LazyExoticComponent<ComponentType>) {
  return (
    <Suspense fallback={<PageFallback />}>
      <Component />
    </Suspense>
  );
}

export function App() {
  const session = useSession();
  const queryClient = useQueryClient();

  if (session.isPending) {
    return (
      <ConfigProvider locale={zhCN}>
        <AntdApp>
          <div style={{ display: 'grid', placeItems: 'center', height: '100vh' }}>
            <Spin size="large" />
          </div>
        </AntdApp>
      </ConfigProvider>
    );
  }

  const authenticated = session.data != null;

  return (
    <ConfigProvider locale={zhCN}>
      <AntdApp>
        {authenticated ? (
          <Routes>
            <Route element={<AppShell />}>
              <Route index element={lazily(ClustersPage)} />
              <Route path="clusters/new" element={lazily(ClustersPage)} />
              <Route path="clusters/:name" element={lazily(ClusterDetailPage)} />
              <Route path="operations" element={lazily(OperationsPage)} />
              <Route path="operations/:id" element={lazily(OperationsPage)} />
              <Route path="packages" element={lazily(PackagesPage)} />
              <Route path="about" element={lazily(AboutPage)} />
              <Route path="*" element={<Navigate to="/" replace />} />
            </Route>
          </Routes>
        ) : (
          <LoginPage onLoggedIn={(info) => queryClient.setQueryData(['session'], info)} />
        )}
      </AntdApp>
    </ConfigProvider>
  );
}
