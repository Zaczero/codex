use super::ChatWidget;
use crate::app_event::AppEvent;

impl ChatWidget {
    pub(super) fn dispatch_parallelism_command(&mut self, args: &str) {
        let parallelism = if args.is_empty() {
            None
        } else {
            let Ok(value) = args.parse::<usize>() else {
                self.add_error_message(
                    "Usage: /parallelism [nonnegative integer]; 0 disables enforcement."
                        .to_string(),
                );
                return;
            };
            Some(value)
        };
        let Some(thread_id) = self.thread_id else {
            self.add_error_message("Start a session before using /parallelism.".to_string());
            return;
        };
        self.app_event_tx.send(AppEvent::TaskParallelism {
            thread_id,
            parallelism,
        });
    }
}
