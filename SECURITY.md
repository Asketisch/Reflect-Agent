# 安全策略

> Reflect 项目的安全漏洞报告与处理流程。本文档面向所有希望负责任地披露
> 漏洞的研究员、用户与贡献者。

---

## 支持的版本

下表列出当前获得安全更新的 Reflect 版本。

| 版本    | 是否支持         |
|---------|------------------|
| 0.0.1   | ✅ 是            |
| < 0.0.1 | ❌ 否            |

> 0.0.1 之前的所有内部版本(0.1.x ~ 1.3 内部代号)未对外发布,不在公开
> 支持范围。请升级到 0.0.1 或更新版本。

---

## 报告漏洞

如果你在 Reflect 中发现安全漏洞,**请不要**通过 GitHub Issues、Discussions
或公开 PR 提交,以免在补丁发布前被恶意利用。

### 私下报告通道

请通过以下任一方式私下联系维护者:

- **邮箱**:`security@reflect-agent.dev`
- **主题前缀**:`[SECURITY]`(便于邮件过滤)
- **加密**(可选):维护者 GPG 公钥见 [`SECURITY_GPG.asc`](SECURITY_GPG.asc)(如有)
  或在邮件中请求

### 报告应包含的信息

为帮助我们快速复现与修复,请尽量提供:

1. **漏洞类型与影响范围**(例如:命令注入、信息泄露、权限绕过)
2. **受影响的组件 / 文件 / 函数 / 配置项**
3. **触发条件与最小复现步骤**(含完整命令、输入、配置)
4. **受影响版本**(根据上表标注的版本号)
5. **已知缓解措施**(若有,包括临时绕过方案)
6. **是否已在公开环境利用**(如已利用,影响面如何)
7. **你的联系方式**(便于后续澄清细节)

### 我们的承诺

- 在收到报告后 **72 小时内** 确认收到
- 在合理时间内(通常 7–30 天,视复杂度)修复并发布补丁
- 修复后通过 [GitHub Security Advisory](https://github.com/CNB/Reflect-Agent/security/advisories)
  公开披露(除非你明确要求匿名)
- 致谢报告者(如果你愿意在公告中署名)
- 修复前不公开披露任何细节

---

## 凭据与隐私

**绝不要**在 issue、PR、commit message、Discussion 或聊天群组中包含:

- 任何 LLM provider 的 API key(OpenAI、Anthropic、Ollama 等)
- 任何 GitHub / GitLab / 平台的个人访问令牌(PAT)
- 任何用户的真实姓名、邮箱、IP、token
- 任何 session 文件内容(可能含敏感上下文)

本项目自带 `secret-sanitize` 工具,会自动检测并脱敏以下常见凭据模式:

- `sk-...` 形式的 OpenAI / Anthropic / 第三方平台 key
- `ghp_...` / `gho_...` 形式的 GitHub token
- Bearer / Basic Authorization 头
- 自定义 provider 配置中的 `api_key` 字段

但请**不要依赖**自动检测 —— 自觉遵守永远是最稳妥的防线。

---

## 公开披露政策

我们遵循 [负责任披露](https://en.wikipedia.org/wiki/Coordinated_vulnerability_disclosure)
原则:

- 收到报告 → 确认 → 调查 → 修复 → 协调公开披露时间
- 默认给报告者 **90 天** 披露窗口期(可协商延长或缩短)
- 若 90 天内未修复且无进展,报告者可自行公开披露(我们鼓励继续协作)

---

## 安全相关更新

- 关注本仓库的 [GitHub Releases](https://github.com/CNB/Reflect-Agent/releases)
  与 [Security Advisories](https://github.com/CNB/Reflect-Agent/security/advisories)
  获取安全公告
- 升级到最新版本是规避已知漏洞的最简单方式

---

## 致谢

感谢所有负责任地披露漏洞、为 Reflect 安全性做出贡献的研究员与用户。

> 本策略参考 [GitHub Security Advisories](https://docs.github.com/en/code-security/security-advisories)、
> [Apache 2.0 安全披露最佳实践](https://www.apache.org/security/) 与
> [CNCF Security TAG](https://github.com/cncf/tag-security) 编写。
