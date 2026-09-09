use super::App;
use crate::app_server_session::AppServerSession;
use crate::markdown_render::render_markdown_lines_with_width_and_cwd;
use crate::pager_overlay::Overlay;
use crate::render::renderable::Renderable;
use crate::terminal_hyperlinks::HyperlinkLine;
use crate::terminal_hyperlinks::HyperlinkParagraph;
use crate::tui::Tui;
use codex_app_server_protocol::ThreadTaskParallelismParams;
use codex_protocol::ThreadId;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::Widget;
use std::cell::Ref;
use std::cell::RefCell;
use std::path::PathBuf;

struct TaskDetails {
    markdown: String,
    cwd: PathBuf,
    rendered: RefCell<(Option<u16>, Vec<HyperlinkLine>)>,
}

impl TaskDetails {
    fn lines(&self, width: u16) -> Ref<'_, [HyperlinkLine]> {
        let mut rendered = self.rendered.borrow_mut();
        if rendered.0 != Some(width) {
            *rendered = (
                Some(width),
                render_markdown_lines_with_width_and_cwd(
                    &self.markdown,
                    Some(usize::from(width)),
                    Some(&self.cwd),
                ),
            );
        }
        drop(rendered);
        Ref::map(self.rendered.borrow(), |rendered| rendered.1.as_slice())
    }
}

impl Renderable for TaskDetails {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.render_scrolled(area, buf, /*scroll_offset*/ 0);
    }

    fn render_scrolled(&self, area: Rect, buf: &mut Buffer, scroll_offset: u16) -> bool {
        HyperlinkParagraph::new(&self.lines(area.width), Style::default())
            .scroll(scroll_offset)
            .render(area, buf);
        true
    }

    fn desired_height(&self, width: u16) -> u16 {
        HyperlinkParagraph::new(&self.lines(width), Style::default())
            .line_count(width)
            .try_into()
            .unwrap_or(u16::MAX)
    }
}

impl App {
    pub(super) async fn task_parallelism(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        parallelism: Option<usize>,
    ) {
        let result = app_server
            .thread_task_parallelism(ThreadTaskParallelismParams {
                thread_id: thread_id.to_string(),
                parallelism,
            })
            .await;
        if self.current_displayed_thread_id() != Some(thread_id) {
            return;
        }
        match result {
            Ok(response) => {
                let message = match response.parallelism {
                    0 => "Task parallelism is off for this session.".to_string(),
                    1 => "Task parallelism is 1 slot for this session.".to_string(),
                    capacity => format!("Task parallelism is {capacity} slots for this session."),
                };
                self.chat_widget.add_info_message(message, /*hint*/ None);
            }
            Err(error) => self
                .chat_widget
                .add_error_message(format!("Could not access task parallelism: {error}")),
        }
    }

    pub(super) async fn open_tasks(
        &mut self,
        tui: &mut Tui,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
    ) {
        match app_server.thread_tasks_read(thread_id).await {
            Ok(response) => {
                let _ = tui.enter_alt_screen();
                self.overlay = Some(Overlay::new_static_with_renderables(
                    vec![Box::new(TaskDetails {
                        markdown: response.text,
                        cwd: self.config.cwd.to_path_buf(),
                        rendered: RefCell::default(),
                    })],
                    "T A S K S".to_string(),
                    self.keymap.pager.clone(),
                ));
                tui.frame_requester().schedule_frame();
            }
            Err(error) => self
                .chat_widget
                .add_error_message(format!("Could not read task ledger: {error}")),
        }
    }
}
