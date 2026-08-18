# security-scan

代码安全审计技能:找常见漏洞类别。

## 扫描类别

- **注入**:SQL(用参数化)、shell(命令拼接)、path traversal(用户输入拼路径)。
- **认证 / 授权**:缺权限检查、JWT 未验签、密码明文存储、session 固定。
- **密钥泄露**:硬编码 secret / token;`.env` 进 git;日志打印凭证。
- **反序列化**:不可信输入反序列化(RON / bincode)触发 RCE。
- **依赖**:CVE(`cargo audit`)、supply chain(可疑新依赖)。
- **unsafe / FFI**:Rust unsafe 块边界、C FFI 内存所有权。

## 流程

1. 列用户输入入口(API / CLI / 文件 / 网络)。
2. 沿数据流追到 sink(执行 / 存储 / 输出),查是否净化。
3. 命中即报告:位置、攻击场景、修复建议。
