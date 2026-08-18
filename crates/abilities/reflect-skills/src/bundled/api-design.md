# api-design

设计清晰、可演进的 API(Rust trait / 公开函数 / HTTP endpoint)。

## 原则

- **最小暴露**:只暴露必要项,内部细节标 `pub(crate)` / 私有。
- **难用错**:类型驱动 —— 用 newtype / enum 让非法状态不可表示。
  - 例:`UserId(NewType)` 而非裸 `String`;`enum State` 而非多个 bool。
- **错误明确**:用 `Result` + 具体错误枚举,别 `Option` + panic。
- **命名一致**:同类操作统一动词(get / list / create / update / delete)。

## 演进

- 加字段 / 加 enum variant 标 `#[non_exhaustive]` 防下游 exhaustive match 破坏。
- 破坏性改动分版本:新增 → deprecate 旧 → 删旧(给迁移窗口)。
