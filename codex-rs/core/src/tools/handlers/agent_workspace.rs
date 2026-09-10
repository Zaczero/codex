use crate::config::Config;
use crate::session::step_context::StepContext;
use codex_file_system::GetMetadataOptions;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_tools::FunctionCallError;
use codex_utils_path_uri::LegacyAppPathString;

pub(super) async fn spawn_environments(
    config: &mut Config,
    step: &StepContext,
    cwd: Option<&str>,
) -> Result<Vec<TurnEnvironmentSelection>, FunctionCallError> {
    let mut selections = step.environments.to_selections();
    let Some(cwd) = cwd else {
        return Ok(selections);
    };
    if cwd.trim().is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "cwd must not be empty".to_owned(),
        ));
    }
    let primary = step.environments.primary().ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "No execution environment is ready for the child cwd".to_owned(),
        )
    })?;
    let cwd = LegacyAppPathString::from_string(cwd)
        .resolve_against(primary.cwd(), primary.user_home_dir.as_ref())
        .map_err(|err| FunctionCallError::RespondToModel(format!("Invalid child cwd: {err}")))?;
    let filesystem = primary.environment.get_filesystem();
    let sandbox = primary.sandbox_context(/*additional_permissions*/ None);
    let metadata = filesystem
        .get_metadata(&cwd, GetMetadataOptions::default(), Some(&sandbox))
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("Cannot access child cwd: {err}"))
        })?;
    if !metadata.is_directory {
        return Err(FunctionCallError::RespondToModel(
            "Child cwd must be a directory".to_owned(),
        ));
    }
    if !primary.environment.is_remote() {
        config.cwd = cwd.to_abs_path().map_err(|err| {
            FunctionCallError::RespondToModel(format!("Invalid local child cwd: {err}"))
        })?;
    }
    selections[0].cwd = cwd;
    Ok(selections)
}
