use super::ThreadRequestProcessor;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ThreadTaskParallelismParams;
use codex_app_server_protocol::ThreadTaskParallelismResponse;
use codex_app_server_protocol::ThreadTasksReadParams;
use codex_app_server_protocol::ThreadTasksReadResponse;

impl ThreadRequestProcessor {
    pub(crate) async fn thread_tasks_read(
        &self,
        params: ThreadTasksReadParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let (_, thread) = self.load_thread(&params.thread_id).await?;
        let ledger = codex_tasks_extension::read(thread.thread_extension_data())
            .await
            .map_err(|error| {
                super::internal_error(format!("could not read task ledger: {error}"))
            })?;
        Ok(Some(
            ThreadTasksReadResponse {
                text: codex_tasks_extension::open_tasks(&ledger),
            }
            .into(),
        ))
    }

    pub(crate) async fn thread_task_parallelism(
        &self,
        params: ThreadTaskParallelismParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let (_, thread) = self.load_thread(&params.thread_id).await?;
        let parallelism =
            codex_tasks_extension::parallelism(thread.thread_extension_data(), params.parallelism)
                .await
                .map_err(|error| match error.kind() {
                    std::io::ErrorKind::InvalidInput => super::invalid_request(error.to_string()),
                    _ => {
                        super::internal_error(format!("could not access task parallelism: {error}"))
                    }
                })?;
        Ok(Some(ThreadTaskParallelismResponse { parallelism }.into()))
    }
}
