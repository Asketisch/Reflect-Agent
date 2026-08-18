//! SSE Redis 回放 stub —— 多实例 Event 流共享占位。

use serde::{Deserialize, Serialize};

/// Redis SSE 后端配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SseRedisConfig {
    pub redis_url: String,
    /// Stream key 前缀。
    #[serde(default = "default_prefix")]
    pub key_prefix: String,
}

fn default_prefix() -> String {
    "reflect:sse:".into()
}

impl Default for SseRedisConfig {
    fn default() -> Self {
        Self {
            redis_url: "redis://127.0.0.1/".into(),
            key_prefix: default_prefix(),
        }
    }
}

/// 后端状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseRedisStubStatus {
    Unconfigured,
    Configured,
}

/// SSE + Redis 回放 stub。
#[derive(Debug, Clone, Default)]
pub struct SseRedisBackend {
    pub config: Option<SseRedisConfig>,
}

impl SseRedisBackend {
    pub fn status(&self) -> SseRedisStubStatus {
        if self
            .config
            .as_ref()
            .is_some_and(|c| !c.redis_url.is_empty())
        {
            SseRedisStubStatus::Configured
        } else {
            SseRedisStubStatus::Unconfigured
        }
    }

    pub fn status_line(&self) -> String {
        match self.status() {
            SseRedisStubStatus::Unconfigured => {
                "sse-redis: stub — 配置 [sse_redis].redis_url 后可用(v2.x 真实化)".into()
            }
            SseRedisStubStatus::Configured => {
                "sse-redis: stub — Redis URL 已配置,Event 回放留 v2.x".into()
            }
        }
    }

    /// 占位:发布 event 到 Redis stream。
    pub fn publish_stub(&self, session_id: &str, event_type: &str) -> String {
        format!("sse-redis stub: publish session={session_id} type={event_type}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_unconfigured() {
        let b = SseRedisBackend::default();
        assert_eq!(b.status(), SseRedisStubStatus::Unconfigured);
    }
}
