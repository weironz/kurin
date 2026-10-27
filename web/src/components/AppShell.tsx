import { Layout, Menu, Button, Typography } from 'antd';
import {
  ClusterOutlined,
  ProfileOutlined,
  AppstoreOutlined,
  InfoCircleOutlined,
  LogoutOutlined,
} from '@ant-design/icons';
import { Outlet, useLocation, useNavigate } from 'react-router-dom';
import { useMutation, useQueryClient } from '@tanstack/react-query';
import { logout } from '../api/client';
import { t } from '../i18n/zh-CN';

const { Header, Sider, Content } = Layout;

/** 侧边栏：集群 / 操作 / 包 / 关于（WP-60）。 */
function navKeyToPath(key: string): string {
  switch (key) {
    case 'clusters':
      return '/';
    case 'operations':
      return '/operations';
    case 'packages':
      return '/packages';
    case 'about':
      return '/about';
    default:
      return '/';
  }
}

function pathToNavKey(pathname: string): string {
  if (pathname.startsWith('/clusters') || pathname === '/') return 'clusters';
  if (pathname.startsWith('/operations')) return 'operations';
  if (pathname.startsWith('/packages')) return 'packages';
  if (pathname.startsWith('/about')) return 'about';
  return 'clusters';
}

export function AppShell() {
  const location = useLocation();
  const navigate = useNavigate();
  const queryClient = useQueryClient();

  const logoutMutation = useMutation({
    mutationFn: logout,
    onSettled: () => {
      // 无论成败都回到登录页
      queryClient.clear();
      window.location.reload();
    },
  });

  return (
    <Layout style={{ minHeight: '100vh' }}>
      <Header
        style={{
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'space-between',
          color: '#fff',
        }}
      >
        <Typography.Title level={4} style={{ color: '#fff', margin: 0 }}>
          {t('app.title')}
        </Typography.Title>
        <Button
          type="text"
          icon={<LogoutOutlined />}
          style={{ color: '#fff' }}
          onClick={() => logoutMutation.mutate()}
          loading={logoutMutation.isPending}
        >
          {t('common.logout')}
        </Button>
      </Header>
      <Layout>
        <Sider theme="light" width={200}>
          <Menu
            mode="inline"
            selectedKeys={[pathToNavKey(location.pathname)]}
            onClick={({ key }) => navigate(navKeyToPath(key))}
            items={[
              { key: 'clusters', icon: <ClusterOutlined />, label: t('nav.clusters') },
              { key: 'operations', icon: <ProfileOutlined />, label: t('nav.operations') },
              { key: 'packages', icon: <AppstoreOutlined />, label: t('nav.packages') },
              { key: 'about', icon: <InfoCircleOutlined />, label: t('nav.about') },
            ]}
            style={{ height: '100%', borderRight: 0, paddingTop: 8 }}
          />
        </Sider>
        <Content style={{ padding: 24 }}>
          <Outlet />
        </Content>
      </Layout>
    </Layout>
  );
}
