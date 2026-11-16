import { expect, it } from 'vitest';
import { compareSemVer } from '../src/semver';
import { marketplaceVersion } from '../src/marketplaceVersion';
import type { CatalogueItem } from '../src/types';

it.each([
  ['0.2.6', '0.4.0'], ['0.9.0', '0.10.0'], ['0.0.9', '0.1.0'], ['1.9.9', '2.0.0'],
  ['9007199254740992.0.0', '9007199254740993.0.0'],
  ['18446744073709551614.0.0', '18446744073709551615.0.0'],
  ['1.0.0-alpha', '1.0.0-alpha.1'], ['1.0.0-alpha.1', '1.0.0-alpha.beta'],
  ['1.0.0-alpha.beta', '1.0.0-beta'], ['1.0.0-beta', '1.0.0-beta.2'],
  ['1.0.0-beta.2', '1.0.0-beta.11'], ['1.0.0-beta.11', '1.0.0-rc.1'], ['1.0.0-rc.1', '1.0.0'],
  ['1.0.0-9999999999999999999999999999', '1.0.0-10000000000000000000000000000'],
  ['1.0.0-9007199254740992', '1.0.0-9007199254740993'],
  ['1.0.0-Z', '1.0.0-a'], ['1.0.0-1', '1.0.0-1a'],
])('严格比较 %s < %s，反向一致', (lower, higher) => {
  expect(compareSemVer(lower, higher)).toBe(-1);
  expect(compareSemVer(higher, lower)).toBe(1);
});

it.each(['0.0.0', '1.2.3', '1.2.3+001', '1.2.3-a-b.0+build.01', '18446744073709551615.0.0'])('合法版本 %s 相等', version => {
  expect(compareSemVer(version, version)).toBe(0);
});
it('build metadata 不参与正式版或预发布优先级', () => {
  expect(compareSemVer('1.2.3+a', '1.2.3+b.002')).toBe(0);
  expect(compareSemVer('1.2.3-rc.1+a', '1.2.3-rc.1')).toBe(0);
});
it.each([
  '', 'v1.2.3', '=1.2.3', '1', '1.2', '1.2.3.4', '01.2.3', '1.02.3', '1.2.03',
  ' 1.2.3', '1.2.3 ', '1.2.3\n', '1.2.3-a\n', '1.2.3+a\n', '1.2.3-01', '1.2.3-a.01',
  '1.2.3-', '1.2.3-a..b', '1.2.3+', '1.2.3+a..b', '1.2.3+a+b', '1.2.3-ä', '1.2.3+a_b',
  '18446744073709551616.0.0', '0.18446744073709551616.0', '0.0.18446744073709551616',
])('拒绝无效版本 %j（任一侧无效均不可比较）', version => {
  expect(compareSemVer(version, '1.2.3')).toBeNull();
  expect(compareSemVer('1.2.3', version)).toBeNull();
});
it('缺失或非字符串版本安全拒绝，不中断市场渲染', () => {
  for (const value of [undefined, null, 123]) expect(compareSemVer('1.2.3', value as unknown as string)).toBeNull();
});

function policy(candidate: string, saved: string | null, loaded: string | null, version = '0.4.0') {
  return marketplaceVersion({ latest: { version: candidate }, installed: { version, saved_version: saved, loaded_version: loaded } } as CatalogueItem);
}
it('只在 saved_version 为 null 时回退 version，不能绕过无效保存版本', () => {
  expect(policy('0.3.0', null, null).allowed).toBe(false);
  expect(policy('0.5.0', null, null).allowed).toBe(true);
  expect(policy('0.5.0', '', null).allowed).toBe(false);
  expect(policy('0.5.0', '0.4.0', null, 'invalid').allowed).toBe(true);
});
it('candidate 等于 loaded 且大于 saved 允许修正 pending；等于 saved 仍拒绝', () => {
  expect(policy('0.4.0+new', '0.2.6', '0.4.0+old')).toMatchObject({ allowed: true, message: expect.stringContaining('修正') });
  expect(policy('0.4.0', '0.4.0', '0.2.6').allowed).toBe(false);
  expect(policy('0.3.0', '0.2.6', '0.4.0').allowed).toBe(false);
});
