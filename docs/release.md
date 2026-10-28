# SDK 发布指南(PyPI / npm)

Python SDK 与 TS SDK 通过 `.github/workflows/release-sdks.yml` 自动发布:
**tag `v*` 驱动 + OIDC Trusted Publishing(仓库 secrets 不存任何 token)**。
CLI 二进制的发布(`release.yml` → GitHub Release)由同一条 tag 一并触发,
两条流水线互不依赖。

## 平台二进制分发(安装即用)

SDK 自 v0.1.x 起随包携带 `zhuye` 平台二进制,`pip install zhuye` /
`npm install zhuye` 后开箱即用。流水线结构(`release-sdks.yml`):

```
gate(版本一致性 + 双 SDK 单测)
  └─ binaries      5 平台编译 zhuye-cli,artifact 共享
       ├─ npm-platform  npm 平台子包 zhuye-<slug>(二进制 + 动态生成清单)→ npm publish
       ├─ pypi          5 平台 wheel(二进制拷入 zhuye/bin/,maturin 自动平台 tag)→ PyPI
       ├─ pypi-sdist    源码分发(不含二进制,保留元数据连续性)
       └─ npm           主包 zhuye(发布前动态注入 optionalDependencies 精确 pin 五个子包)
```

- **npm 平台子包**:`zhuye-darwin-arm64` / `darwin-x64` / `linux-x64` /
  `linux-arm64` / `win32-x64`,清单由 CI 动态生成(`os`/`cpu` 字段让
  npm 按平台选择性安装),**源码不维护清单文件**;主包
  `optionalDependencies` 同样发布时注入,四处版本联动机制不受影响。
- **Python 平台 wheel**:构建后端为 maturin(`sdks/python/pyproject.toml`,
  `bindings = "bin"` + `manifest-path` 指向 `bindings-stub` 形式占位
  crate),二进制以内嵌包数据(`zhuye/bin/`)单份携带;平台 tag 由
  maturin 按构建宿主自动生成。sdist 保留元数据连续性,但**不含二进制、
  不可独立编译**(Rust workspace 依赖),预编译平台一律用 wheel。
- 解析优先级(双 SDK 一致):`options.bin` → `ZHUYE_BIN` → 内嵌二进制
  → PATH 上的 `zhuye`;平台不匹配(如 musl)时回退 PATH。
- 本地 `release_sdk.sh check` 构建的是无内嵌二进制的开发产物,仅验证
  元数据;平台产物一律由 CI 产出。

四处版本统一管理(单一版本号贯穿 Rust workspace / Python / TS),
用 `scripts/release_sdk.sh` 维护:

```bash
./scripts/release_sdk.sh check        # 版本一致性 + 本地构建双包 + 元数据检查(不发布)
./scripts/release_sdk.sh bump 0.2.0   # 四处版本同步 bump(幂等),记得提交改动
./scripts/release_sdk.sh tag 0.2.0    # 正式发布:打 v0.2.0 并 push(⚠️ 触发下面两条流水线)
```

## 测试阶段(当前状态)

**已发布**:`zhuye v0.1.1` 起双 registry 正式发布(PyPI 项目由 Pending
Publisher 自动创建;npm 走 Trusted Publisher OIDC)。平台侧均已配置,
`v*` tag 即正式发布动作。

改动流水线本身时,仍建议先走 dry-run 路径验证(不发布):

1. 本地跑 `./scripts/release_sdk.sh check`,确认双包构建与元数据检查通过;
2. 推送后在 GitHub **Actions → release-sdks → Run workflow** 手动触发,
   `dry_run` 保持默认 `true`;
3. 检查 run 的三个 job(gate / PyPI / npm)全绿,并下载
   `pypi-dist`、`npm-pack` 两个 artifact 人工核对包内容与元数据。

## 正式发布清单(包名已定为 `zhuye`)

### 第 0 步:包名

PyPI / npm 同名 **`zhuye`**(首发 v0.1.1 已验证两条 OIDC 链路)。
发布名与 Python 导入名(`import zhuye`)、npm 导入名
(`import { ZhuyeAgent } from 'zhuye'`)一致。

### 第 1 步:PyPI 侧配置(Pending Publisher)

已配置(首发即由此自动建项目)。若将来需要重建,五元组必须与
workflow 精确一致:

   - PyPI project name:`zhuye`
   - Owner:`Asketisch` / Repository:`ZhuYe`
   - Workflow filename:`release-sdks.yml`
   - Environment:`pypi`

### 第 2 步:npm 侧配置(Trusted Publisher)

已配置(v0.1.1 起全部走 CI 的 OIDC 发布)。若将来需要重建:Trusted
Publisher 登记在**包详情页**的设置里,要求包已存在(首次需手动发一版
`npm publish`,账号开 2FA 时会交互式索要动态码);登记时填 Owner
`Asketisch` / Repository `ZhuYe` / Workflow `release-sdks.yml` /
Environment `npm`,保存后 **2 天内必须完成一次成功发布**,否则配置过期。

### 第 3 步:发布

```bash
./scripts/release_sdk.sh bump X.Y.Z && git commit -m "build: 版本对齐 X.Y.Z"
./scripts/release_sdk.sh check
./scripts/release_sdk.sh tag vX.Y.Z    # 有交互确认;脚本会校验 clean tree + 版本一致
```

推 tag 后在 Actions 页观察 `release`(CLI → GitHub Release)与
`release-sdks`(SDK → PyPI / npm)两个 workflow。任一 registry 发布失败,
修复后单独重跑 `release-sdks` 的对应 job 即可(tag 不必重打)。

## 改名 checklist

包名已定为 `zhuye`,本节仅供将来改名时参考。包名/仓库名变更时,需要
同步的位置(流水线与脚本不写死包名,无需改动):

| 位置 | 文件 / 操作 |
|---|---|
| PyPI 包名 | `sdks/python/pyproject.toml` 的 `[project] name` |
| npm 包名 | `sdks/typescript/package.json` 的 `name` |
| 仓库地址(三处) | 根 `Cargo.toml` `[workspace.package] repository`、`pyproject.toml` `[project.urls]`、`package.json` `repository`/`homepage`/`bugs` |
| Python 包导入名(如涉及) | `sdks/python/zhuye/` 目录名 + `pyproject.toml` `[tool.setuptools]` include |
| 本文档 | 上述第 1/2 步登记用的名字 |
| 平台侧 | PyPI Pending Publisher / npm Trusted Publisher 按新名重新登记 |

注意:名字一旦在正式 PyPI / npm 发布即**占用**,改名等于放弃旧名
(npm 新版 72 小时内可 unpublish 释放,PyPI 不可)。

## 流水线自带的安装冒烟

`release-sdks.yml` 在构建检查之外内置多层安装验证,捕获「构建过但装不上 /
缺文件 / 入口导入失败 / 内嵌二进制不可执行」类问题:

- **发布前(dry-run 也跑)**:每个平台 wheel 在干净 venv 安装后,断言
  内嵌二进制可解析且 `--version` 可执行;npm 对 pack 出的真实 tgz 在
  干净目录安装并 ESM `import('zhuye')` 断言 `ZhuyeAgent` 导出;
- **发布后(仅正式发布)**:分别从 PyPI / npm registry 真实安装
  `zhuye==<tag 版本>`(npm 连同平台子包),双端断言内嵌二进制解析与
  `--version` 执行,Python 侧额外断言 `__version__` 与 tag 一致
  (registry CDN 有传播延迟,npm 侧重试窗口 10×20s)。失败时可在
  Actions 页单独重跑对应 job,tag 不必重打。
