# perf-profile

性能问题定位与优化。

## 流程

1. **测量先行**:不要盲优化。先用 benchmark / profiler 找热点。
   - 微基准:`cargo bench` / `scripts/bench.sh`。
   - 火焰图:`cargo flamegraph` 看 CPU 热点。
   - 分配:`dhat` / `cargo instruments` 看堆分配。
2. **定位瓶颈**:区分 CPU / 内存 / IO / 锁竞争。
3. **假设优化**:针对热点改一处(算法 / 缓存 / 批量 / 减分配)。
4. **复测对比**:改完再跑同一 benchmark,确认提升且无回归。

## 优先级

算法 > 数据结构 > 缓存 > 批量 > 微优化(循环展开等)。
先砍大数,再磨小数。
