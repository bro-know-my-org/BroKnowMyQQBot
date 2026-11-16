interface Version { core: string[]; pre: string[] }
const numeric = /^[0-9]+$/;
const coreNumber = /^(0|[1-9][0-9]*)$/;
const identifier = /^[0-9A-Za-z-]+$/;
const U64_MAX = '18446744073709551615';
function compareText(a: string, b: string): number {
  if (a === b) return 0;
  return a < b ? -1 : 1;
}
// Decimal strings avoid rounding both u64 core values and unbounded prerelease numbers.
function compareNumber(a: string, b: string): number {
  if (a.length === b.length) return compareText(a, b);
  return a.length < b.length ? -1 : 1;
}

function parse(version: string): Version | null {
  if (typeof version !== 'string' || /[^0-9A-Za-z.+-]/.test(version)) return null;
  const buildParts = version.split('+');
  if (buildParts.length > 2 || (buildParts.length === 2 && !buildParts[1].split('.').every(part => identifier.test(part)))) return null;
  const base = buildParts[0], dash = base.indexOf('-');
  const core = (dash < 0 ? base : base.slice(0, dash)).split('.');
  if (core.length !== 3 || core.some(part => !coreNumber.test(part) || compareNumber(part, U64_MAX) > 0)) return null;
  const pre = dash < 0 ? [] : base.slice(dash + 1).split('.');
  if (pre.some(part => !identifier.test(part) || (numeric.test(part) && !coreNumber.test(part)))) return null;
  return { core, pre };
}

/** Strict SemVer 2 precedence, with Rust semver's u64 core range; null means invalid. */
export function compareSemVer(a: string, b: string): number | null {
  const left = parse(a), right = parse(b);
  if (!left || !right) return null;
  for (let i = 0; i < 3; i++) {
    const order = compareNumber(left.core[i], right.core[i]);
    if (order) return order;
  }
  if (!left.pre.length || !right.pre.length) {
    if (left.pre.length === right.pre.length) return 0;
    return left.pre.length ? -1 : 1;
  }
  for (let i = 0; i < Math.min(left.pre.length, right.pre.length); i++) {
    const l = left.pre[i], r = right.pre[i], ln = numeric.test(l), rn = numeric.test(r);
    let order: number;
    if (ln && rn) order = compareNumber(l, r);
    else if (ln !== rn) order = ln ? -1 : 1;
    else order = compareText(l, r);
    if (order) return order;
  }
  return Math.sign(left.pre.length - right.pre.length);
}
