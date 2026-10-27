# SDK 发布指南(PyPI / npm)

Python SDK 与 TS SDK 通过 `.github/workflows/release-sdks.yml` 自动发布:
**tag `v*` 驱动 + OIDC Trusted Publishing(仓库 secrets 不存任何 token)**。
CLI 二进制的发布(`release.yml` → GitHub Release)由同一条 tag 一并触发,
两条流水线互不依赖。

三处版本统一管理(单一版本号贯穿 Rust workspace / Python / TS),
用 `scripts/release_sdk.sh` 维护:

```bash
./scripts/release_sdk.sh check        # 版本一致性 + 本地构建双包 + 元数据检查(不发布)
./scripts/release_sdk.sh bump 0.2.0   # 三处版本同步 bump(幂等),记得提交改动
./scripts/release_sdk.sh tag 0.2.0    # 正式发布:打 v0.2.0 并 push(⚠️ 触发下面两条流水线)
```

## 测试阶段(当前状态)

平台侧(PyPI / npm 账号与 Trusted Publishing 登记)**尚未配置**,因此现在
只验证流水线的 dry-run 路径,**不发布、不占用任何包名**:

1. 本地跑 `./scripts/release_sdk.sh check`,确认双包构建与元数据检查通过;
2. 推送后在 GitHub **Actions → release-sdks → Run workflow** 手动触发,
   `dry_run` 保持默认 `true`;
3. 检查 run 的三个 job(gate / PyPI / npm)全绿,并下载
   `pypi-dist`、`npm-pack` 两个 artifact 人工核对包内容与元数据。

⚠️ 测试阶段**不要推 `v*` tag**:那会同时触发 `release.yml`(CLI 发公开
GitHub Release)与 `release-sdks.yml`(SDK 真发布;平台未配置时会失败)。

## 正式发布清单(包名定下来后)

### 第 0 步:确定包名

当前包名 `zhuye-agent`(PyPI / npm 同名)。若项目改名,先走
[改名 checklist](#改名-checklist),平台登记用最终名字做,避免返工。

### 第 1 步:PyPI 侧配置(Pending Publisher)

1. 注册 [pypi.org](https://pypi.org) 账号;
2. 账号设置 → Publishing → 添加 **Pending Publisher**(项目尚未存在也可预登记,
   首次发布即自动建项目),五元组必须与 workflow 精确一致:
   - PyPI project name:`zhuye-agent`(或改名后的最终名)
   - Owner:`Asketisch` / Repository:`ZhuYe`
   - Workflow filename:`release-sdks.yml`
   - Environment:`pypi`

### 第 2 步:npm 侧配置(Trusted Publisher)

npm 的 Trusted Publisher 登记在**包详情页**的设置里,要求包已存在,因此
首版需先手动发一次(此后全部走 CI):

```bash
cd sdks/typescript && npm whoami   # npm ≥ 11.5.1,先 npm login
npm publish                        # 首版手动发布
```

然后在 npmjs.com 包设置页 → Trusted Publisher 登记(保存后 **2 天内必须完成
一次成功发布**,否则配置过期需重建):Owner `Asketisch` / Repository
`ZhuYe` / Workflow `release-sdks.yml` / Environment `npm`。

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

包名/仓库名变更时,需要同步的位置(流水线与脚本不写死包名,无需改动):

| 位置 | 文件 / 操作 |
|---|---|
| PyPI 包名 | `sdks/python/pyproject.toml` 的 `[project] name` |
| npm 包名 | `sdks/typescript/package.json` 的 `name` |
| 仓库地址(三处) | 根 `Cargo.toml` `[workspace.package] repository`、`pyproject.toml` `[project.urls]`、`package.json` `repository`/`homepage`/`bugs` |
| Python 包导入名(如涉及) | `sdks/python/zhuye/` 目录名 + `pyproject.toml` `[tool.setuptools]` include |
| 本文档 | 上述第 1/2 步登记用的名字 |
| 平台侧 | PyPI Pending Publisher / npm Trusted Publisher 按新名重新登记 |

注意:名字一旦在正式 PyPI / npm 发布即**占用**,改名等于放弃旧名
(npm 新版 72 小时内可 unpublish 释放,PyPI 不可)——这也是正式发布
推迟到改名决策之后的原因。
