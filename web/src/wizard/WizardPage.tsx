import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { useQuery } from '@tanstack/react-query';
import { Alert, Button, Card, Steps, Typography, message } from 'antd';
import { apiFetch } from '../api/client';
import type { AddonCatalogEntry, Diagnostic, Meta, PackageInfo } from '../api/types';
import { ensureClusterRegistered, renderAndValidate, submitOperation } from '../api/wizard';
import { t } from '../i18n/zh-CN';
import {
  AuthStep,
  BasicStep,
  ComponentsStep,
  ConfirmStep,
  NetworkStep,
  NodesStep,
  OfflineStep,
  ReviewStep,
  TemplateStep,
  type ReviewState,
  type StepProps,
} from './steps';
import { buildDto } from './dto';
import {
  STEPS,
  STEP_TITLE_KEYS,
  addonClosure,
  indexCapabilities,
  stepBlocked,
  type WizardForm,
} from './model';
import { emptyForm } from './model';

/** 每步 render+validate 的产物（去抖后最新一次）。 */
interface StepValidation {
  toml: string;
  valid: boolean;
  diagnostics: Diagnostic[];
}

/** 诊断列表（422 的 labels 在这里只作为 message 的补充）。 */
function DiagnosticList({ diagnostics }: { diagnostics: Diagnostic[] }) {
  return (
    <ul style={{ margin: 0, paddingLeft: 18 }}>
      {diagnostics.map((d, i) => (
        <li key={i}>
          <Typography.Text type="danger">{d.message}</Typography.Text>
          {d.help ? <Typography.Text type="secondary"> — {d.help}</Typography.Text> : null}
        </li>
      ))}
    </ul>
  );
}

/** 创建向导 `/clusters/new`（UI §8.2 的 9 步）。 */
export default function WizardPage() {
  const navigate = useNavigate();
  const [step, setStep] = useState(0);
  const [form, setForm] = useState<WizardForm>(emptyForm);
  const [validation, setValidation] = useState<StepValidation | null>(null);
  const [validating, setValidating] = useState(false);
  /** TOML 模式（单向）：`tomlText` 是唯一事实，表单不再回填。 */
  const [tomlText, setTomlText] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const [submitError, setSubmitError] = useState<string | null>(null);
  const [acknowledged] = useState(false);
  const [review, setReview] = useState<ReviewState>({
    precheck: null,
    precheckError: null,
    precheckRunning: false,
    plan: null,
    planError: null,
    planRunning: false,
    acknowledged: false,
  });
  const [messageApi, contextHolder] = message.useMessage();

  const metaQuery = useQuery({
    queryKey: ['meta'],
    queryFn: () => apiFetch<Meta>('/meta'),
    staleTime: Infinity,
  });
  const catalogQuery = useQuery({
    queryKey: ['addons-catalog'],
    queryFn: () => apiFetch<{ addons: AddonCatalogEntry[] }>('/addons/catalog'),
    staleTime: Infinity,
  });
  const packagesQuery = useQuery({
    queryKey: ['packages'],
    queryFn: () => apiFetch<{ packages: PackageInfo[] }>('/packages'),
    staleTime: 30_000,
  });

  const catalog = useMemo(() => catalogQuery.data?.addons ?? [], [catalogQuery.data]);
  const capabilities = useMemo(
    () => indexCapabilities(metaQuery.data?.capabilities ?? []),
    [metaQuery.data],
  );
  const selectedPackage = useMemo(
    () => (packagesQuery.data?.packages ?? []).find((p) => p.id === form.packageId) ?? null,
    [packagesQuery.data, form.packageId],
  );
  const offlineSelected = form.packageMode !== 'online';

  useEffect(() => {
    document.title = `${t('page.clusters.newTitle')} · kurin`;
  }, []);

  const updateForm = useCallback((patch: Partial<WizardForm>) => {
    setForm((prev) => ({ ...prev, ...patch }));
  }, []);

  /* ------------------------- 每步 render + validate ------------------------- */
  const dto = useMemo(
    () => buildDto(form, catalog, selectedPackage),
    [form, catalog, selectedPackage],
  );
  const renderSeq = useRef(0);
  useEffect(() => {
    // UI §8.2 规则 1：每一步都 render + validate；去抖避免打字风暴。
    const seq = (renderSeq.current += 1);
    const timer = setTimeout(() => {
      setValidating(true);
      renderAndValidate(dto)
        .then((result) => {
          if (seq !== renderSeq.current) return;
          setValidation({
            toml: result.toml,
            valid: result.validate.valid,
            diagnostics: result.validate.diagnostics,
          });
        })
        .catch(() => {
          // 网络/400：不阻塞导航，显示为「未校验」。
          if (seq === renderSeq.current) setValidation(null);
        })
        .finally(() => {
          if (seq === renderSeq.current) setValidating(false);
        });
    }, 350);
    return () => clearTimeout(timer);
  }, [dto]);

  /* ------------------------------- 检查页门控 ------------------------------- */
  const precheckFailed = (review.precheck?.failed ?? 0) > 0 || review.precheckError != null;
  const warningsUnacked =
    (review.precheck?.warned ?? 0) > 0 && !(review.acknowledged || acknowledged);
  const createBlocked = precheckFailed || warningsUnacked;
  /** 第 8 步要求先跑过 precheck（UI §8.2「有失败项时创建不可用」）。 */
  const reviewReady = review.precheck != null;

  /* --------------------------------- 提交 --------------------------------- */
  const submitCreate = async () => {
    setSubmitting(true);
    setSubmitError(null);
    try {
      const toml = form.tomlMode ? tomlText : (validation?.toml ?? '');
      await ensureClusterRegistered(form.name, toml);
      const submitted = await submitOperation(form.name, 'create');
      messageApi.info(t('wizard.confirm.submitted'));
      navigate(`/operations/${submitted.id}`);
    } catch (error) {
      setSubmitError(
        t('wizard.confirm.submitFailed', {
          message: error instanceof Error ? error.message : String(error),
        }),
      );
    } finally {
      setSubmitting(false);
    }
  };

  /* ------------------------------- 步骤渲染 ------------------------------- */
  const blockedReason = stepBlocked(STEPS[step], form);
  const stepProps: StepProps = {
    form,
    updateForm,
    capabilities,
    catalog,
    meta: metaQuery.data,
    packages: packagesQuery.data?.packages ?? [],
    offlineSelected,
  };
  const currentToml = validation?.toml ?? '';
  const addonNames = Object.keys(form.addons);
  const requiredAddons = useMemo(() => [...addonClosure(catalog, addonNames)], [catalog, addonNames]);
  const packageLabel =
    form.packageMode === 'online'
      ? t('wizard.offline.mode.online')
      : selectedPackage != null
        ? `${selectedPackage.name} ${selectedPackage.version}（${selectedPackage.source}）`
        : form.packageUri;

  const steps = [
    <TemplateStep key="template" {...stepProps} />,
    <BasicStep key="basic" {...stepProps} />,
    <NodesStep key="nodes" {...stepProps} />,
    <AuthStep key="auth" {...stepProps} />,
    <NetworkStep key="network" {...stepProps} />,
    <ComponentsStep key="components" {...stepProps} />,
    <OfflineStep key="offline" {...stepProps} />,
    <ReviewStep
      key="review"
      form={form}
      toml={currentToml}
      state={review}
      setState={(patch) => setReview((prev) => ({ ...prev, ...patch }))}
    />,
    <ConfirmStep
      key="confirm"
      form={form}
      toml={currentToml}
      tomlMode={form.tomlMode}
      tomlText={tomlText}
      onEnterTomlMode={() => {
        setTomlText(currentToml);
        updateForm({ tomlMode: true });
      }}
      onTomlChange={setTomlText}
      addonNames={[...new Set([...requiredAddons, ...addonNames])]}
      packageLabel={packageLabel}
    />,
  ];

  const atReview = step === 7;
  const atConfirm = step === 8;

  return (
    <div>
      {contextHolder}
      <Typography.Title level={3}>{t('page.clusters.newTitle')}</Typography.Title>
      <Typography.Paragraph type="secondary">{t('wizard.description')}</Typography.Paragraph>
      <Steps
        current={step}
        size="small"
        items={STEPS.map((s) => ({ title: t(STEP_TITLE_KEYS[s]) }))}
        style={{ marginBottom: 24 }}
      />
      {steps[step]}
      <Card size="small" style={{ marginTop: 16 }}>
        {validating ? (
          <Typography.Text type="secondary">{t('wizard.rendering')}</Typography.Text>
        ) : validation == null ? null : validation.valid ? (
          <Alert type="success" showIcon message={t('wizard.valid')} />
        ) : (
          <Alert
            type="error"
            showIcon
            message={t('wizard.validationErrors', { n: validation.diagnostics.length })}
            description={<DiagnosticList diagnostics={validation.diagnostics} />}
          />
        )}
      </Card>
      <div style={{ marginTop: 16, display: 'flex', justifyContent: 'space-between' }}>
        <Button onClick={() => setStep(Math.max(0, step - 1))} disabled={step === 0}>
          {t('wizard.prev')}
        </Button>
        {atConfirm ? (
          <Button
            type="primary"
            loading={submitting}
            disabled={createBlocked || !reviewReady || (validation?.valid ?? false) === false}
            onClick={submitCreate}
            data-testid="wizard-create"
          >
            {t('wizard.create')}
          </Button>
        ) : (
          <Button
            type="primary"
            onClick={() => setStep(step + 1)}
            disabled={
              blockedReason != null ||
              (atReview && (createBlocked || !reviewReady))
            }
            title={blockedReason == null ? undefined : t(blockedReason)}
            data-testid="wizard-next"
          >
            {atReview ? t('wizard.create') : t('wizard.next')}
          </Button>
        )}
      </div>
      {atReview && !reviewReady ? (
        <Alert
          type="info"
          showIcon
          message={t('wizard.review.precheckRun')}
          style={{ marginTop: 12 }}
        />
      ) : null}
      {submitError ? (
        <Alert type="error" showIcon message={submitError} style={{ marginTop: 12 }} />
      ) : null}
    </div>
  );
}
