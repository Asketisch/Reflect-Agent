# git-conflict-resolve

解决 merge / rebase 冲突。

## 流程

1. **看全貌**:`git status` 列所有冲突文件;`git log --oneline` 理解两边意图。
2. **逐文件**:打开冲突标记 `<<<<<<<` / `=======` / `>>>>>>>`。
3. **理解双方**:读 ours 与 theirs 各自为什么这么改(查 commit message)。
4. **合并意图**:不是选一边,而是融合两边真正想做的事;若语义冲突问 author。
5. **验证**:解决后 `cargo check` + `cargo test`;确认未丢任一方逻辑。
6. **标记**:`git add` 已解决文件;rebase 用 `git rebase --continue`。

## 注意

- 生成文件(lockfile / 编译产物)冲突:重新生成而非手并。
- 同行不同改动优先理解;删代码冲突确认是有意删还是合并遗漏。
