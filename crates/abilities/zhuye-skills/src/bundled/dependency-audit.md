# dependency-audit

审计项目依赖:版本陈旧 / 安全漏洞 / 许可证 / 未使用。

## 流程

1. **清单**:`cargo tree` / `Cargo.lock` 拉全量依赖图。
2. **陈旧**:`cargo outdated` 找落后版本;评估升级风险(breaking / patch)。
3. **安全**:`cargo audit` 扫已知 CVE;有漏洞立即升级或加补丁版本约束。
4. **许可证**:确认所有依赖许可证与项目兼容(Apache-2.0 / MIT 兼容表)。
5. **未使用**:`cargo udeps` / `cargo machete` 找未引用依赖,清理。

## 注意

- 升级 major 版本前读 CHANGELOG,准备适配。
- supply chain:可疑新增依赖查 crate 作者 / 下载量 / repo 活跃度。
