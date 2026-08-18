//! Team 命名约定 + 校验 helper + `TeamMemberSpec ↔ SubAgentSpec` 转换。
//!
//! agent id 格式固定为 `team-lead@<team>`;`parse_agent_id`
//! 反向解析。`validate_team_name` 与 `reflect-subagent::SubAgentSpec::validate`
//! 的角色字符规则保持一致(`[a-z0-9_-]+`),保证两类 spec 可互相转换。
//!
//! Phase 3 落地 `From<&TeamMemberSpec> for SubAgentSpec` —— 由
//! `TaskManager::sync_team_specs` 把团队成员转成可 spawn 的 subagent spec,
//! 注入到 `SubAgentFactory.dynamic_specs`。

use reflect_subagent::DataTransferConfig;
use reflect_subagent::SubAgentSpec;

use crate::error::TaskError;
use crate::model::TeamMemberSpec;

/// 构造 lead agent id(固定 `team-lead@<team>` 形式)。
///
/// # Example
/// ```ignore
/// assert_eq!(lead_agent_id_for("rocket"), "team-lead@rocket");
/// ```
pub fn lead_agent_id_for(team: &str) -> String {
    format!("team-lead@{team}")
}

/// 反向解析 agent id,返回 `Some((role, team))` 或 `None`(不含 `@`)。
///
/// 用于 [`crate::model::TeamMemberSpec`] 与 [`reflect_subagent::SubAgentSpec`]
/// 的转换(`role` 部分对应 `SubAgentSpec.role`)。
pub fn parse_agent_id(s: &str) -> Option<(&str, &str)> {
    s.split_once('@')
}

/// 校验 team 名合法性:1..=64 字符,字符集 `[a-z0-9_-]`。
///
/// 与 `reflect-subagent::SubAgentSpec::validate` 的角色规则对齐,避免
/// 在 `TeamMemberSpec → SubAgentSpec` 转换时再做一次校验。
pub fn validate_team_name(name: &str) -> Result<(), TaskError> {
    if name.is_empty() {
        return Err(TaskError::Invalid("team name cannot be empty".into()));
    }
    if name.len() > 64 {
        return Err(TaskError::Invalid(
            "team name length must be 1..=64 chars".into(),
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return Err(TaskError::Invalid(
            "team name must match [a-z0-9_-]+".into(),
        ));
    }
    Ok(())
}

/// 把团队成员转成可 spawn 的 subagent spec。
///
/// 字段映射规则:
/// - `role` 解析自 `agent_id` 的 `<role>@<team>` 前缀(必须含 `@`)。
/// - `name` 直传(`team-lead` / `architect` / `builder`)。
/// - `model` / `system_prompt` / `allowed_tools` 直传。
/// - `data_transfer` 默认 `DataTransferConfig::default()`(M6 接入后调整)。
///
/// # Panics
///
/// 若 `agent_id` 不含 `@`(违反 `TeamCreateTool::parse_member` 强校验),
/// 调用 `parse_agent_id` 拿到 `None` → 退化为用 `agent_id` 全文做 role,
/// 这通常会立即被 `SubAgentSpec::validate` 拒收。
impl From<&TeamMemberSpec> for SubAgentSpec {
    fn from(member: &TeamMemberSpec) -> Self {
        // role 优先取 `agent_id` 的 `<role>@<team>` 前缀,失败回退到 `member.role`。
        // 不 panic:agent_id 校验已在 TeamCreateTool::parse_member 阶段完成,这里
        // 看到的 member 一定含 `@`,但仍写兜底以防 phase 0/1 数据走旁路写入。
        let role = parse_agent_id(&member.agent_id)
            .map(|(r, _)| r.to_string())
            .unwrap_or_else(|| member.role.clone());
        SubAgentSpec {
            name: member.name.clone(),
            role,
            model: member.model.clone(),
            system_prompt: member.system_prompt.clone(),
            allowed_tools: member.allowed_tools.clone(),
            data_transfer: DataTransferConfig::default(),
            max_turns: None,
            allowed_skills: vec![],
        }
    }
}

/// `From<TeamMemberSpec>` 转发给 `From<&TeamMemberSpec>`,便于 caller
/// 持有 owned `TeamMemberSpec` 时直接 `.into()`。
impl From<TeamMemberSpec> for SubAgentSpec {
    fn from(member: TeamMemberSpec) -> Self {
        SubAgentSpec::from(&member)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lead_agent_id_format() {
        assert_eq!(lead_agent_id_for("rocket"), "team-lead@rocket");
        assert_eq!(
            lead_agent_id_for("rocket-launch_v1"),
            "team-lead@rocket-launch_v1"
        );
    }

    #[test]
    fn parse_agent_id_roundtrip() {
        assert_eq!(
            parse_agent_id("team-lead@rocket"),
            Some(("team-lead", "rocket"))
        );
        assert_eq!(
            parse_agent_id("architect@rocket"),
            Some(("architect", "rocket"))
        );
    }

    #[test]
    fn parse_agent_id_no_at_returns_none() {
        assert_eq!(parse_agent_id("no-at-here"), None);
        assert_eq!(parse_agent_id(""), None);
    }

    #[test]
    fn validate_accepts_legal_names() {
        for ok in ["a", "rocket", "rocket-launch", "team_v1", "abc-123_xyz"] {
            assert!(validate_team_name(ok).is_ok(), "should accept '{ok}'");
        }
    }

    #[test]
    fn validate_rejects_empty() {
        assert!(validate_team_name("").is_err());
    }

    #[test]
    fn validate_rejects_too_long() {
        let long = "a".repeat(65);
        assert!(validate_team_name(&long).is_err());
    }

    #[test]
    fn validate_rejects_uppercase() {
        assert!(validate_team_name("Rocket").is_err());
    }

    #[test]
    fn validate_rejects_spaces() {
        assert!(validate_team_name("rocket launch").is_err());
    }

    #[test]
    fn validate_rejects_punctuation() {
        for bad in ["rocket!", "rocket@v1", "rocket/v1", "rocket.v1"] {
            assert!(validate_team_name(bad).is_err(), "should reject '{bad}'");
        }
    }

    #[test]
    fn validate_accepts_max_length() {
        let s = "a".repeat(64);
        assert!(validate_team_name(&s).is_ok());
    }

    // ── Phase 3: TeamMemberSpec → SubAgentSpec 转换 ──

    fn make_member(agent_id: &str, name: &str, role: &str) -> TeamMemberSpec {
        use std::time::SystemTime;
        TeamMemberSpec {
            agent_id: agent_id.into(),
            name: name.into(),
            role: role.into(),
            model: Some("anthropic/claude-opus-4-6".into()),
            system_prompt: "design".into(),
            allowed_tools: vec!["Read".into(), "Write".into()],
            color: None,
            joined_at: SystemTime::now(),
            session_id: None,
            subscriptions: vec![],
        }
    }

    /// `agent_id` 解析为 role;name / model / system_prompt / allowed_tools 直传。
    #[test]
    fn team_member_to_subagent_basic_mapping() {
        let m = make_member("architect@rocket", "architect", "architect");
        let s: SubAgentSpec = (&m).into();
        assert_eq!(s.role, "architect");
        assert_eq!(s.name, "architect");
        assert_eq!(s.model.as_deref(), Some("anthropic/claude-opus-4-6"));
        assert_eq!(s.system_prompt, "design");
        assert_eq!(s.allowed_tools, vec!["Read".to_string(), "Write".into()]);
        // validate 通过(角色字符集合规)。
        assert!(s.validate().is_ok());
    }

    /// lead member 同样能转换,role = `team-lead`。
    #[test]
    fn team_member_to_subagent_lead_role() {
        let m = make_member("team-lead@rocket", "team-lead", "team-lead");
        let s: SubAgentSpec = (&m).into();
        assert_eq!(s.role, "team-lead");
        assert_eq!(s.name, "team-lead");
        assert!(s.validate().is_ok());
    }

    /// `agent_id` 异常(不含 @)→ 回退到 `member.role`。
    #[test]
    fn team_member_to_subagent_fallback_role() {
        let m = make_member("orphan-id", "x", "fallback-role");
        let s: SubAgentSpec = (&m).into();
        assert_eq!(s.role, "fallback-role");
    }

    /// `From<TeamMemberSpec>` 与 `From<&TeamMemberSpec>` 输出等价。
    #[test]
    fn team_member_owned_and_borrow_into_equivalent() {
        let m = make_member("builder@rocket", "builder", "builder");
        let owned_spec: SubAgentSpec = m.clone().into();
        let borrow_spec: SubAgentSpec = (&m).into();
        assert_eq!(owned_spec.role, borrow_spec.role);
        assert_eq!(owned_spec.name, borrow_spec.name);
        assert_eq!(owned_spec.system_prompt, borrow_spec.system_prompt);
        assert_eq!(owned_spec.allowed_tools, borrow_spec.allowed_tools);
    }
}
