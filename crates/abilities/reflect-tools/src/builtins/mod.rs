//! agent 自带的内置工具集。

pub mod ask_user;
pub mod ask_user_question;
pub mod bash;
pub mod brief;
pub mod context_remaining;
pub mod delete;
pub mod echo;
pub mod edit;
pub mod enter_plan_mode;
pub mod enter_worktree;
pub mod exit_plan_mode;
pub mod exit_worktree;
pub mod glob;
pub mod grep;
pub mod image_view;
pub mod notebook_edit;
pub mod plan_write;
pub mod read;
pub mod request_human_input;
pub mod tool_search;
pub mod url_safety;
pub mod web_fetch;
pub mod web_search;
pub mod write;

pub use ask_user::AskUserTool;
pub use ask_user_question::AskUserQuestionTool;
pub use bash::BashTool;
pub use brief::BriefTool;
pub use context_remaining::GetContextRemainingTool;
pub use delete::DeleteTool;
pub use echo::EchoTool;
pub use edit::EditTool;
pub use enter_plan_mode::EnterPlanModeTool;
pub use enter_worktree::EnterWorktreeTool;
pub use exit_plan_mode::ExitPlanModeTool;
pub use exit_worktree::ExitWorktreeTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use image_view::ImageViewTool;
pub use notebook_edit::NotebookEditTool;
pub use plan_write::PlanWriteTool;
pub use read::ReadTool;
pub use request_human_input::RequestHumanInputTool;
pub use tool_search::ToolSearchTool;
pub use web_fetch::WebFetchTool;
pub use web_search::WebSearchTool;
pub use write::WriteTool;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tool;

    #[test]
    fn all_builtin_descriptions_are_loaded_from_prompt_resources() {
        let tool_search = ToolSearchTool::new(std::sync::Arc::new(crate::ToolRegistry::new()));
        let web_fetch = WebFetchTool::default();
        let web_search = WebSearchTool::new();
        let ask_uq = AskUserQuestionTool::default();
        let descriptions = [
            AskUserTool.description(),
            ask_uq.description(),
            BashTool.description(),
            BriefTool.description(),
            GetContextRemainingTool.description(),
            DeleteTool.description(),
            EchoTool.description(),
            EditTool.description(),
            EnterPlanModeTool.description(),
            EnterWorktreeTool.description(),
            ExitPlanModeTool.description(),
            ExitWorktreeTool.description(),
            GlobTool.description(),
            GrepTool.description(),
            ImageViewTool.description(),
            NotebookEditTool.description(),
            PlanWriteTool.description(),
            ReadTool.description(),
            RequestHumanInputTool.description(),
            tool_search.description(),
            web_fetch.description(),
            web_search.description(),
            WriteTool.description(),
        ];
        assert_eq!(descriptions.len(), 23);
        assert!(
            descriptions
                .iter()
                .all(|description| !description.is_empty())
        );
    }
}
