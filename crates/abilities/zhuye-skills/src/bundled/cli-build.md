# cli-build

构建 ergonomic CLI(子命令 / flag / 帮助)。

## 原则

- **discoverable**:`--help` 自解释;子命令分组;有例子。
- **consistent**:flag 风格统一(`--long` + 缩写 `-l`);退出码语义一致(0 成功 / 非 0 失败)。
- **fail well**:错误信息说"哪里错了 + 怎么修",不 dump panic 栈给用户。

## 结构

- 顶层:`app [--global-flag] <command> [command-args]`。
- 子命令:每个聚焦一件事(run / build / config / doctor)。
- 输入:flag 用于可选配置,位置参数用于必需主对象。
- 输出:默认人类可读,`--json` 切机器可读(脚本友好)。

## 检查清单

- [ ] `--help` / `-h` 都在?
- [ ] `--version`?
- [ ] 子命令有 `app help <cmd>`?
- [ ] 错误退出码区分(usage 错误 vs 运行时错误)?
