import { useState } from 'react';
import { Card, Form, Input, Button, Alert, Typography, Space } from 'antd';
import { loginWithToken, ApiError } from '../api/client';
import type { SessionInfo } from '../api/types';
import { t } from '../i18n';

interface LoginForm {
  token: string;
}

/** 登录页（UI §6.10）：通常经 hash 链接自动登录；此表单是手动粘贴令牌的后备。 */
export function LoginPage({ onLoggedIn }: { onLoggedIn: (info: SessionInfo) => void }) {
  const [error, setError] = useState<string | null>(null);

  async function handleSubmit(values: LoginForm) {
    setError(null);
    try {
      const info = await loginWithToken(values.token.trim());
      onLoggedIn(info);
    } catch (err) {
      setError(
        err instanceof ApiError && err.isUnauthorized
          ? t('login.failed')
          : `${t('common.error')}: ${err instanceof Error ? err.message : String(err)}`,
      );
    }
  }

  return (
    <div
      style={{
        minHeight: '100vh',
        display: 'grid',
        placeItems: 'center',
        background: '#f5f5f5',
      }}
    >
      <Card style={{ width: 420 }}>
        <Space direction="vertical" size="large" style={{ width: '100%' }}>
          <div>
            <Typography.Title level={3} style={{ margin: 0 }}>
              {t('login.title')}
            </Typography.Title>
            <Typography.Paragraph type="secondary">{t('login.description')}</Typography.Paragraph>
          </div>
          {error && <Alert type="error" showIcon message={error} />}
          <Form<LoginForm> layout="vertical" onFinish={handleSubmit}>
            <Form.Item
              name="token"
              label={t('login.tokenLabel')}
              rules={[{ required: true, message: t('login.tokenLabel') }]}
            >
              <Input.Password autoComplete="off" />
            </Form.Item>
            <Button type="primary" htmlType="submit" block>
              {t('login.submit')}
            </Button>
          </Form>
        </Space>
      </Card>
    </div>
  );
}
