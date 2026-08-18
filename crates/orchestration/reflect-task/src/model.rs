//! `Task` / `Team` 数据模型。
//!
//! 四态模型(`pending` / `in_progress` / `completed` /
//! `deleted`),`blocks` / `blockedBy` 双向依赖,`owner` 归属)。Team
//! 文件采用 `team-lead@<team>` 作为 lead agent id 约定。
//!
//! 时间字段(`created_at` / `updated_at`)用 `SystemTime` 而非 `chrono`,
//! 与 `reflect-rollout::RolloutRecord` 保持一致(避免拖入额外依赖)。

use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// 任务 ID 1-based,由高水位文件分配;单调递增,跨 list 独立计数。
pub type TaskId = u32;

/// 任务列表 ID;worker 视角 = session id,team 视角 = team 名。
///
/// 类型别名而不是新类型,允许 caller 直接传 `&str` 转换。约束通过
/// `TaskManager::create_task` 等方法强制。
pub type ListId = String;

/// Team 名,合法字符 `[a-z0-9_-]+`,长度 1..=64;由 [`validate_team_name`] 校验。
pub type TeamName = String;

/// 任务状态机。四态:
/// - `Pending` — 已创建尚未开始。
/// - `InProgress` — 已被某 owner 领取。
/// - `Completed` — 已完成。
/// - `Deleted` — 软删除(文件仍保留,`list` 过滤时排除)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
    Deleted,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            TaskStatus::Pending => "pending",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::Completed => "completed",
            TaskStatus::Deleted => "deleted",
        })
    }
}

/// 单个任务。on-disk 格式 = 整个结构序列化为 pretty JSON。
///
/// `claimed_by` / `claimed_at` 是 v1.1.0 Phase 4 加的「worker 自动认领」
/// 字段:区别于 `owner` 的纯标签语义,`claimed_by` 由 `TaskClaim` 工具
/// 原子写入,记录 worker id(`<role>@<team>` 或 session id),并把
/// `status` 从 `Pending` 推到 `InProgress`。`TaskRelease` 工具把这两个
/// 字段清空,任务回 `Pending`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub list_id: ListId,
    pub subject: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub status: TaskStatus,
    /// 本任务阻塞的 task id 列表(下游)。
    pub blocks: Vec<TaskId>,
    /// 阻塞本任务的上游 task id 列表(下游完成后才能开始)。
    pub blocked_by: Vec<TaskId>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    /// 任务输出文件路径(由 TaskCreate 自动分配,TaskOutput 读回)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_path: Option<PathBuf>,
    /// 原子认领的 worker id(`TaskClaim` 写入,`TaskRelease` 清空)。
    /// 与 `owner` 不同:`owner` 是 LLM 自由改的语义标签,`claimed_by` 只由
    /// 协调器 worker 协议改,后者触发 `TaskUpdated` 钩子。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_by: Option<String>,
    /// `claimed_by` 的写入时间戳。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_at: Option<SystemTime>,
    pub created_at: SystemTime,
    pub updated_at: SystemTime,
}

/// 团队成员规格 —— 描述一个可以被 `SubAgentFactory` spawn 的 agent。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamMemberSpec {
    /// Agent id,格式 `<role>@<team>`,lead agent 固定为 `team-lead@<team>`。
    pub agent_id: String,
    /// 人类可读名(如 `"team-lead"` / `"architect"` / `"builder"`)。
    pub name: String,
    /// 角色,kebab-case;沿用 [`crate::team::validate_team_name`] 字符规则。
    #[serde(default = "default_member_role")]
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub system_prompt: String,
    /// 成员被允许使用的父级 tool 名字列表;空 = 无工具(text-only)。
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    pub joined_at: SystemTime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// 订阅的 channel 列表(M9 阶段使用,Phase 2 留空)。
    #[serde(default)]
    pub subscriptions: Vec<String>,
}

fn default_member_role() -> String {
    "member".into()
}

/// Team 文件 —— on-disk 格式,持久化到 `~/.reflect/teams/<name>.json`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamFile {
    pub name: TeamName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 固定 `team-lead@<name>` 形式;由 [`crate::team::lead_agent_id_for`] 生成。
    pub lead_agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead_session_id: Option<String>,
    pub members: Vec<TeamMemberSpec>,
    pub created_at: SystemTime,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_status_uses_snake_case() {
        assert_eq!(
            serde_json::to_string(&TaskStatus::Pending).unwrap(),
            "\"pending\""
        );
        assert_eq!(
            serde_json::to_string(&TaskStatus::InProgress).unwrap(),
            "\"in_progress\""
        );
        assert_eq!(
            serde_json::to_string(&TaskStatus::Completed).unwrap(),
            "\"completed\""
        );
        assert_eq!(
            serde_json::to_string(&TaskStatus::Deleted).unwrap(),
            "\"deleted\""
        );
    }

    #[test]
    fn task_status_serde_roundtrip() {
        for s in [
            TaskStatus::Pending,
            TaskStatus::InProgress,
            TaskStatus::Completed,
            TaskStatus::Deleted,
        ] {
            let j = serde_json::to_string(&s).unwrap();
            let back: TaskStatus = serde_json::from_str(&j).unwrap();
            assert_eq!(s, back);
        }
    }

    #[test]
    fn task_status_display_matches_serde() {
        for s in [
            TaskStatus::Pending,
            TaskStatus::InProgress,
            TaskStatus::Completed,
            TaskStatus::Deleted,
        ] {
            assert_eq!(
                s.to_string(),
                serde_json::to_string(&s).unwrap().trim_matches('"')
            );
        }
    }

    #[test]
    fn task_minimal_serde_roundtrip() {
        // 缺失 optional 字段(无 active_form / owner / output_path / claimed_by /
        // claimed_at)能正确序列化。
        let now = SystemTime::UNIX_EPOCH;
        let t = Task {
            id: 1,
            list_id: "session-x".into(),
            subject: "write tests".into(),
            description: "add unit tests for store".into(),
            active_form: None,
            owner: None,
            status: TaskStatus::Pending,
            blocks: vec![],
            blocked_by: vec![],
            metadata: serde_json::json!({}),
            output_path: None,
            claimed_by: None,
            claimed_at: None,
            created_at: now,
            updated_at: now,
        };
        let j = serde_json::to_string(&t).unwrap();
        let back: Task = serde_json::from_str(&j).unwrap();
        assert_eq!(back.id, 1);
        assert_eq!(back.subject, "write tests");
        assert_eq!(back.status, TaskStatus::Pending);
        assert!(back.active_form.is_none());
        assert!(back.claimed_by.is_none());
        assert!(back.claimed_at.is_none());
    }

    /// v1.1.0 之前持久化的 JSON 没有 `claimed_by` / `claimed_at` 字段,
    /// 反序列化时必须回退到 `None` 而非失败(serde default 兜底)。
    ///
    /// 通过对一份无 claim 字段的 Task 做序列化-反序列化 roundtrip 验证。
    #[test]
    fn task_legacy_json_without_claim_fields_deserializes() {
        let now = SystemTime::UNIX_EPOCH;
        let legacy = Task {
            id: 7,
            list_id: "session-x".into(),
            subject: "legacy".into(),
            description: "no claim fields".into(),
            active_form: None,
            owner: None,
            status: TaskStatus::Pending,
            blocks: vec![],
            blocked_by: vec![],
            metadata: serde_json::json!({}),
            output_path: None,
            claimed_by: None,
            claimed_at: None,
            created_at: now,
            updated_at: now,
        };
        // 模拟 v1.0 的 on-disk 形态:序列化前手动剥掉新字段。
        let v = serde_json::to_value(&legacy).unwrap();
        let mut obj = v.as_object().unwrap().clone();
        obj.remove("claimed_by");
        obj.remove("claimed_at");
        let legacy_json = serde_json::to_string(&obj).unwrap();
        let back: Task = serde_json::from_str(&legacy_json).unwrap();
        assert_eq!(back.id, 7);
        assert!(back.claimed_by.is_none());
        assert!(back.claimed_at.is_none());
        assert_eq!(back.status, TaskStatus::Pending);
    }

    /// `claimed_by` 设值后,序列化字段出现,反序列化能 roundtrip。
    #[test]
    fn task_with_claimed_by_roundtrip() {
        let now = SystemTime::UNIX_EPOCH;
        let t = Task {
            id: 2,
            list_id: "session-x".into(),
            subject: "claimed".into(),
            description: "".into(),
            active_form: None,
            owner: None,
            status: TaskStatus::InProgress,
            blocks: vec![],
            blocked_by: vec![],
            metadata: serde_json::json!({}),
            output_path: None,
            claimed_by: Some("architect@rocket".into()),
            claimed_at: Some(now),
            created_at: now,
            updated_at: now,
        };
        let j = serde_json::to_string(&t).unwrap();
        assert!(j.contains("\"claimed_by\":\"architect@rocket\""));
        let back: Task = serde_json::from_str(&j).unwrap();
        assert_eq!(back.claimed_by.as_deref(), Some("architect@rocket"));
        assert_eq!(back.claimed_at, Some(now));
    }

    #[test]
    fn team_file_minimal_serde_roundtrip() {
        let now = SystemTime::UNIX_EPOCH;
        let f = TeamFile {
            name: "rocket".into(),
            description: Some("Build rocket".into()),
            lead_agent_id: "team-lead@rocket".into(),
            lead_session_id: None,
            members: vec![],
            created_at: now,
        };
        let j = serde_json::to_string(&f).unwrap();
        let back: TeamFile = serde_json::from_str(&j).unwrap();
        assert_eq!(back.name, "rocket");
        assert_eq!(back.lead_agent_id, "team-lead@rocket");
    }
}
