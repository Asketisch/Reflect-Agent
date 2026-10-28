//! 内嵌二进制解析:定位随平台子包(optionalDependencies)安装的
//! `zhuye` 可执行文件,让 `npm install zhuye` 后开箱即用。
//!
//! 解析优先级见 `agent.ts` 的 spawn 链:options.bin → ZHUYE_BIN →
//! 本模块 → PATH 上的 `zhuye`。

import { accessSync, constants } from 'node:fs';
import { createRequire } from 'node:module';
import { join } from 'node:path';

/** process.platform → process.arch → npm 平台子包名(与发布矩阵一一对应)。 */
const PLATFORM_PACKAGES: Record<string, Record<string, string>> = {
  darwin: { arm64: 'zhuye-darwin-arm64', x64: 'zhuye-darwin-x64' },
  linux: { arm64: 'zhuye-linux-arm64', x64: 'zhuye-linux-x64' },
  win32: { x64: 'zhuye-win32-x64' },
};

const EXE_SUFFIX = process.platform === 'win32' ? '.exe' : '';

/** 当前平台对应的 npm 平台子包名;不在支持矩阵时返回 null。 */
export function platformPackageName(
  platform: string = process.platform,
  arch: string = process.arch,
): string | null {
  return PLATFORM_PACKAGES[platform]?.[arch] ?? null;
}

/** 由平台子包目录推导二进制路径(纯函数拆分,便于单测)。 */
export function binaryPathFromPackageDir(pkgDir: string): string {
  return join(pkgDir, 'bin', `zhuye${EXE_SUFFIX}`);
}

/**
 * 解析随包安装的内嵌 `zhuye` 二进制的绝对路径。
 *
 * 平台子包未落地(不支持的平台,或 optionalDependencies 安装失败 ——
 * npm 对 optional 依赖失败静默容忍)时返回 null,由 spawn 链回退到
 * PATH 上的 `zhuye`。
 *
 * 用 createRequire 而非 import.meta.resolve:后者在旧 Node 需要
 * experimental 标志;createRequire 自 Node 12.2 稳定,且天然解析
 * node_modules 的真实落盘路径。
 */
export function resolveBundledBinary(): string | null {
  const pkg = platformPackageName();
  if (pkg === null) return null;
  try {
    const require = createRequire(import.meta.url);
    const manifest = require.resolve(`${pkg}/package.json`);
    const bin = binaryPathFromPackageDir(join(manifest, '..'));
    accessSync(bin, constants.X_OK);
    return bin;
  } catch {
    return null;
  }
}
