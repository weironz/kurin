/**
 * 语义化版本判定（WP-65）：升级对话框的目标版本在提交前先过一遍
 * 与服务端 `semver::Version::parse` 一致的形状检查（可选 `v` 前缀）。
 */

const SEMVER_RE =
  /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-((?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*)(?:\.(?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*))*))?(?:\+([0-9a-zA-Z-]+(?:\.[0-9a-zA-Z-]+)*))?$/;

/** 是否为合法 semver（允许 `v` 前缀，两端空白忽略）。 */
export function isValidSemver(version: string): boolean {
  const text = version.trim().replace(/^v/, '');
  return text !== '' && SEMVER_RE.test(text);
}
