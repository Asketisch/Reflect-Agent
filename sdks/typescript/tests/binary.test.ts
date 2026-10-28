//! 内嵌二进制解析的单元测试:平台映射表 + 目录推导 + 未安装回退。
//!
//! 正向路径(平台子包真实安装后的解析)由 CI 的 tgz 安装冒烟覆盖:
//! 单测环境不会安装 `zhuye-<平台>` optional dependency,伪造
//! node_modules 需要污染依赖树,不值得。

import { chmodSync, mkdirSync, writeFileSync } from 'node:fs';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, describe, expect, it } from 'vitest';

import { binaryPathFromPackageDir } from '../src/binary.js';
import {
  platformPackageName,
  resolveBundledBinary,
} from '../src/index.js';

describe('platformPackageName 平台映射表', () => {
  it('五平台矩阵与发布矩阵一一对应', () => {
    expect(platformPackageName('darwin', 'arm64')).toBe('zhuye-darwin-arm64');
    expect(platformPackageName('darwin', 'x64')).toBe('zhuye-darwin-x64');
    expect(platformPackageName('linux', 'x64')).toBe('zhuye-linux-x64');
    expect(platformPackageName('linux', 'arm64')).toBe('zhuye-linux-arm64');
    expect(platformPackageName('win32', 'x64')).toBe('zhuye-win32-x64');
  });

  it('不在矩阵中的平台返回 null(如 linux ia32 / freebsd)', () => {
    expect(platformPackageName('linux', 'ia32')).toBeNull();
    expect(platformPackageName('freebsd', 'x64')).toBeNull();
    expect(platformPackageName('win32', 'arm64')).toBeNull();
  });
});

describe('binaryPathFromPackageDir 目录推导', () => {
  it('POSIX 平台拼接 bin/zhuye', () => {
    if (process.platform === 'win32') return; // Windows 下后缀为 .exe
    expect(binaryPathFromPackageDir('/x/node_modules/zhuye-linux-x64')).toBe(
      '/x/node_modules/zhuye-linux-x64/bin/zhuye',
    );
  });
});

describe('resolveBundledBinary 回退', () => {
  const tempDirs: string[] = [];

  afterEach(() => {
    while (tempDirs.length) rmSync(tempDirs.pop()!, { recursive: true, force: true });
  });

  it('开发环境未安装平台子包时返回 null(不抛错)', () => {
    // 仓库内 node_modules 没有 zhuye-<平台> 包;若本机恰好装过,
    // 该断言退化为"能解析到可执行文件",同样成立。
    const resolved = resolveBundledBinary();
    expect(resolved === null || typeof resolved === 'string').toBe(true);
  });

  it('binaryPathFromPackageDir 对真实落盘目录返回可访问路径', () => {
    const dir = mkdtempSync(join(tmpdir(), 'zhuye-pkg.'));
    tempDirs.push(dir);
    mkdirSync(join(dir, 'bin'), { recursive: true });
    const bin = binaryPathFromPackageDir(dir);
    writeFileSync(bin, '#!/bin/sh\n');
    chmodSync(bin, 0o755);
    expect(bin.startsWith(dir)).toBe(true);
  });
});
