import { useEffect } from 'react';
import { Typography } from 'antd';
import { t } from '../i18n/zh-CN';

/** `/packages` 占位（WP-65 实现列表、导入、校验、推送）。 */
export default function PackagesPage() {
  useEffect(() => {
    document.title = `${t('page.packages.title')} · kurin`;
  }, []);

  return (
    <div>
      <Typography.Title level={3}>{t('page.packages.title')}</Typography.Title>
      <Typography.Paragraph type="secondary">{t('page.packages.description')}</Typography.Paragraph>
    </div>
  );
}
