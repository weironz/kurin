/**
 * addon 参数表单（UI §8.2 第 6 步）：由 values JSON Schema 生成（rjsf + antd）。
 *
 * addon 目录的 schema 是 draft 2020-12（`$defs`/`$ref`），需要 Ajv 2020 实例；
 * 值的合法性最终仍由服务端在 render/validate 判定，这里只做即时反馈。
 */
import Ajv2020 from 'ajv/dist/2020';
import { customizeValidator } from '@rjsf/validator-ajv8';
import { Form as RjsfForm } from '@rjsf/antd';
import type { RJSFSchema } from '@rjsf/utils';
import { t } from '../i18n';

/** draft-2020 校验器（模块级单例：跨 addon 复用编译缓存）。 */
const ajv2020Validator = customizeValidator({ AjvClass: Ajv2020 });

export interface AddonValuesFormProps {
  /** addon 的 values JSON Schema（`{}` 表示无参数）。 */
  schema: Record<string, unknown>;
  values: Record<string, unknown>;
  onChange: (values: Record<string, unknown>) => void;
}

/** 无属性 schema → 不渲染表单。 */
export function AddonValuesForm({ schema, values, onChange }: AddonValuesFormProps) {
  const properties = schema.properties as Record<string, unknown> | undefined;
  if (properties == null || Object.keys(properties).length === 0) return null;
  return (
    <div data-testid="wizard-addon-values" style={{ maxWidth: 560 }}>
      <RjsfForm
        schema={schema as RJSFSchema}
        validator={ajv2020Validator}
        formData={values}
        onChange={(event: { formData?: unknown }) =>
          onChange((event.formData as Record<string, unknown>) ?? {})
        }
        children
      />
      <div style={{ fontSize: 12, color: '#888', marginTop: 4 }}>
        {t('wizard.components.addonValues')}
      </div>
    </div>
  );
}
