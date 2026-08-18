# dockerize

为应用构建生产级容器镜像。

## 原则

- **多阶段**:builder 阶段装编译工具,最终阶段只拷二进制 + 运行时依赖,镜像小。
- **最小基础**:用 distroless / alpine / scratch;非 root 用户运行。
- **缓存层**:先拷 `Cargo.toml` + `Cargo.lock` 预热依赖,再拷源码(源码变动不废依赖层)。
- **构建参数**:`--build-arg` 注入版本 / feature;secret 不进镜像(用 buildkit secret)。

## 检查清单

- [ ] `.dockerignore` 排除 target / .git / node_modules?
- [ ] 镜像跑 `cargo build --release`(非 debug)?
- [ ] EXPOSE + ENTRYPOINT 正确;非 root?
- [ ] 健康检查(HEALTHCHECK)配置?
