# migrate-database

数据库 schema 迁移技能(增量、可回滚)。

## 原则

- **向前 + 向后兼容**:迁移分两阶段 deploy(扩)→ 后续 release(用)→ 再 deploy(删旧)。
- **幂等**:迁移可重复跑不报错(用 IF NOT EXISTS / 检查表)。
- **可回滚**:每个 up migration 配对应 down;CI 验证 up→down→up 一致。
- **小步**:一次迁移只改一件事(加列 / 加索引 / 改类型),大改拆多步。

## 风险操作

- 加 NOT NULL 列:先加 nullable → 回填 → 改 NOT NULL。
- 改列类型:新建列 → 双写 → 迁移 → 切读 → 删旧列。
- 加索引大表:CONCURRENTLY(不锁表)。
