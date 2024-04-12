mod completion_provider;

use anyhow::Result;
use client::Client;
use completion_provider::*;
use editor::Editor;
use futures::StreamExt;
use gpui::{
    list, prelude::*, AnyElement, AppContext, Global, ListAlignment, ListState, Model, Render,
    Task, View,
};
use language::{language_settings::SoftWrap, LanguageRegistry};
use project::Fs;
use rich_text::RichText;
use semantic_index::ProjectIndex;
use serde::Deserialize;
use settings::Settings;
use std::sync::Arc;
use theme::ThemeSettings;
use ui::{popover_menu, prelude::*, ButtonLike, Color, ContextMenu, Tooltip};
use util::ResultExt;

// gpui::actions!(assistant, [Submit]);

#[derive(Eq, PartialEq, Copy, Clone, Deserialize)]
pub struct Submit(SubmitMode);

/// There are multiple different ways to submit a model request, represented by this enum.
#[derive(Eq, PartialEq, Copy, Clone, Deserialize)]
pub enum SubmitMode {
    /// Only include the conversation.
    Simple,
    /// Send the current file as context.
    CurrentFile,
    /// Search the codebase and send relevant excerpts.
    Codebase,
}

gpui::impl_actions!(assistant, [Submit]);

pub fn init(client: Arc<Client>, cx: &mut AppContext) {
    cx.set_global(CompletionProvider::new(CloudCompletionProvider::new(
        client,
    )));
}

pub struct AssistantPanel {
    language_registry: Arc<LanguageRegistry>,
    project_index: Model<ProjectIndex>,
    fs: Arc<dyn Fs>,
    chat: View<AssistantChat>,
}

impl AssistantPanel {
    pub fn new(
        language_registry: Arc<LanguageRegistry>,
        project_index: Model<ProjectIndex>,
        fs: Arc<dyn Fs>,
        cx: &mut ViewContext<Self>,
    ) -> Self {
        let chat = cx.new_view(|cx| {
            AssistantChat::new(
                language_registry.clone(),
                project_index.clone(),
                fs.clone(),
                cx,
            )
        });
        Self {
            language_registry,
            project_index,
            fs,
            chat,
        }
    }
}

impl Render for AssistantPanel {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        div()
            .size_full()
            .v_flex()
            .p_2()
            .bg(cx.theme().colors().background)
            .child(self.chat.clone())
    }
}

struct AssistantChat {
    model: String,
    messages: Vec<ChatMessage>,
    list_state: ListState,
    language_registry: Arc<LanguageRegistry>,
    project_index: Model<ProjectIndex>,
    fs: Arc<dyn Fs>,
    next_message_id: MessageId,
    next_context_id: ContextId,
    pending_completion: Option<Task<()>>,
}

impl AssistantChat {
    fn new(
        language_registry: Arc<LanguageRegistry>,
        project_index: Model<ProjectIndex>,
        fs: Arc<dyn Fs>,
        cx: &mut ViewContext<Self>,
    ) -> Self {
        let this = cx.view().downgrade();
        let list_state = ListState::new(0, ListAlignment::Bottom, px(1024.), move |ix, cx| {
            this.update(cx, |this, cx| this.render_message(ix, cx))
                .unwrap()
        });
        let model = CompletionProvider::get(cx).default_model();

        let mut this = Self {
            model,
            messages: Vec::new(),
            list_state,
            language_registry,
            project_index,
            fs,
            next_message_id: MessageId(0),
            next_context_id: ContextId(0),
            pending_completion: None,
        };
        this.push_new_user_message(true, cx);
        this
    }

    fn focused_message_id(&self, cx: &WindowContext) -> Option<MessageId> {
        self.messages.iter().find_map(|message| match message {
            ChatMessage::User(message) => message
                .body
                .focus_handle(cx)
                .contains_focused(cx)
                .then_some(message.id),
            ChatMessage::Assistant(_) => None,
        })
    }

    fn submit(&mut self, Submit(mode): &Submit, cx: &mut ViewContext<Self>) {
        let Some(focused_message_id) = self.focused_message_id(cx) else {
            log::error!("unexpected state: no user message editor is focused.");
            return;
        };

        self.truncate_messages(focused_message_id, cx);
        self.push_new_assistant_message(cx);

        let populate = self.populate_context_on_submit(focused_message_id, mode, cx);

        self.pending_completion = Some(cx.spawn(|this, mut cx| async move {
            let complete = async {
                populate.await?;

                let completion = this.update(&mut cx, |this, cx| {
                    CompletionProvider::get(cx).complete(
                        this.model.clone(),
                        this.completion_messages(cx),
                        Vec::new(),
                        1.0,
                    )
                });

                let mut stream = completion?.await?;

                let mut body = String::new();

                while let Some(chunk) = stream.next().await {
                    let chunk = chunk?;
                    this.update(&mut cx, |this, cx| {
                        if let Some(ChatMessage::Assistant(AssistantMessage {
                            body: message_body,
                            ..
                        })) = this.messages.last_mut()
                        {
                            body.push_str(&chunk);
                            *message_body =
                                RichText::new(body.clone(), &[], &this.language_registry);
                            cx.notify();
                        } else {
                            unreachable!()
                        }
                    })?;
                }

                anyhow::Ok(())
            }
            .await;

            this.update(&mut cx, |this, cx| {
                if let Err(error) = complete {
                    if let Some(ChatMessage::Assistant(AssistantMessage {
                        error: message_error,
                        ..
                    })) = this.messages.last_mut()
                    {
                        message_error.replace(SharedString::from(error.to_string()));
                        cx.notify();
                    } else {
                        unreachable!()
                    }
                }

                let focus = this
                    .user_message(focused_message_id)
                    .body
                    .focus_handle(cx)
                    .contains_focused(cx);
                this.push_new_user_message(focus, cx);
            })
            .log_err();
        }));
    }

    fn populate_context_on_submit(
        &mut self,
        submitted_id: MessageId,
        mode: &SubmitMode,
        cx: &mut ViewContext<Self>,
    ) -> Task<Result<()>> {
        // Want to mutate the user message that initiated

        match mode {
            SubmitMode::Simple => return Task::ready(Ok(())),
            SubmitMode::CurrentFile => return Task::ready(Ok(())),
            SubmitMode::Codebase => {}
        }

        let context_id = self.next_context_id.post_inc();
        self.user_message(submitted_id)
            .contexts
            .push(AssistantContext::codebase(context_id));

        let query = self.user_message(submitted_id).body.read(cx).text(cx);
        let results = self.project_index.read(cx).search(&query, 4, cx);
        let fs = self.fs.clone();

        cx.spawn(|this, mut cx| async move {
            let results = results.await;

            let excerpts = results.into_iter().map(|result| {
                let abs_path = result
                    .worktree
                    .read_with(&cx, |worktree, _| worktree.abs_path().join(&result.path));
                let fs = fs.clone();

                async move {
                    let path = result.path.clone();
                    let text = fs.load(&abs_path?).await?;
                    let text = SharedString::from(text[result.range].to_string());

                    anyhow::Ok(CodebaseExcerpt {
                        path: path.to_string_lossy().to_string().into(),
                        text,
                        score: result.score,
                    })
                }
            });

            let excerpts = futures::future::join_all(excerpts).await;

            this.update(&mut cx, |this, cx| {
                this.codebase_context(submitted_id, context_id).populate(
                    excerpts
                        .into_iter()
                        .filter_map(|result| result.log_err())
                        .collect(),
                );
                cx.notify();
            })?;

            anyhow::Ok(())
        })
    }

    fn user_message(&mut self, message_id: MessageId) -> &mut UserMessage {
        self.messages
            .iter_mut()
            .find_map(|message| match message {
                ChatMessage::User(user_message) if user_message.id == message_id => {
                    Some(user_message)
                }
                _ => None,
            })
            .expect("User message not found")
    }

    fn codebase_context(
        &mut self,
        message_id: MessageId,
        context_id: ContextId,
    ) -> &mut CodebaseContext {
        self.messages
            .iter_mut()
            .find_map(|message| match message {
                ChatMessage::User(user_message) if user_message.id == message_id => user_message
                    .contexts
                    .iter_mut()
                    .find_map(|context| match context {
                        AssistantContext::Codebase(context) => {
                            if context.id == context_id {
                                Some(context)
                            } else {
                                None
                            }
                        }
                    }),
                _ => None,
            })
            .expect("Codebase context not found")
    }

    fn push_new_user_message(&mut self, focus: bool, cx: &mut ViewContext<Self>) {
        let message = ChatMessage::User(UserMessage {
            id: self.next_message_id.post_inc(),
            body: cx.new_view(|cx| {
                let mut editor = Editor::auto_height(80, cx);
                editor.set_soft_wrap_mode(SoftWrap::EditorWidth, cx);
                if focus {
                    cx.focus_self();
                }
                editor
            }),
            contexts: Vec::new(),
        });
        self.push_message(message, cx);
    }

    fn push_new_assistant_message(&mut self, cx: &mut ViewContext<Self>) {
        let message = ChatMessage::Assistant(AssistantMessage {
            id: self.next_message_id.post_inc(),
            body: RichText::default(),
            error: None,
        });
        self.push_message(message, cx);
    }

    fn push_message(&mut self, message: ChatMessage, cx: &mut ViewContext<Self>) {
        let old_len = self.messages.len();
        self.messages.push(message);
        self.list_state.splice(old_len..old_len, 1);
        cx.notify();
    }

    fn truncate_messages(&mut self, last_message_id: MessageId, cx: &mut ViewContext<Self>) {
        if let Some(index) = self.messages.iter().position(|message| match message {
            ChatMessage::User(message) => message.id == last_message_id,
            ChatMessage::Assistant(message) => message.id == last_message_id,
        }) {
            self.list_state.splice(index + 1..self.messages.len(), 0);
            self.messages.truncate(index + 1);
            cx.notify();
        }
    }

    fn render_error(
        &self,
        error: Option<SharedString>,
        ix: usize,
        cx: &mut ViewContext<Self>,
    ) -> AnyElement {
        let theme = cx.theme();

        if let Some(error) = error {
            div()
                .py_1()
                .px_2()
                .neg_mx_1()
                .rounded_md()
                .border()
                .border_color(theme.status().error_border)
                // .bg(theme.status().error_background)
                .text_color(theme.status().error)
                .child(error.clone())
                .into_any_element()
        } else {
            div().into_any_element()
        }
    }

    fn render_message(&self, ix: usize, cx: &mut ViewContext<Self>) -> AnyElement {
        let is_last = ix == self.messages.len() - 1;

        match &self.messages[ix] {
            ChatMessage::User(UserMessage { body, contexts, .. }) => div()
                .on_action(cx.listener(Self::submit))
                .p_2()
                .when(!is_last, |element| element.mb_2())
                .text_color(cx.theme().colors().editor_foreground)
                .font(ThemeSettings::get_global(cx).buffer_font.clone())
                .bg(cx.theme().colors().editor_background)
                .child(body.clone())
                .children(contexts.iter().map(|context| context.render(cx)))
                .into_any(),
            ChatMessage::Assistant(AssistantMessage { id, body, error }) => div()
                .when(!is_last, |element| element.mb_2())
                .child(div().p_2().child(body.element(ElementId::from(id.0), cx)))
                .child(self.render_error(error.clone(), ix, cx))
                .into_any(),
        }
    }

    fn completion_messages(&self, cx: &WindowContext) -> Vec<CompletionMessage> {
        self.messages
            .iter()
            .map(|message| match message {
                ChatMessage::User(UserMessage { body, contexts, .. }) => CompletionMessage {
                    role: CompletionRole::User,
                    body: body.read(cx).text(cx),
                },
                ChatMessage::Assistant(AssistantMessage { body, .. }) => CompletionMessage {
                    role: CompletionRole::Assistant,
                    body: body.text.to_string(),
                },
            })
            .collect()
    }

    fn render_model_dropdown(&self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        let this = cx.view().downgrade();
        div().h_flex().justify_end().child(
            div().w_32().child(
                popover_menu("user-menu")
                    .menu(move |cx| {
                        ContextMenu::build(cx, |mut menu, cx| {
                            for model in CompletionProvider::get(cx).available_models() {
                                menu = menu.custom_entry(
                                    {
                                        let model = model.clone();
                                        move |_| Label::new(model.clone()).into_any_element()
                                    },
                                    {
                                        let this = this.clone();
                                        move |cx| {
                                            _ = this.update(cx, |this, cx| {
                                                this.model = model.clone();
                                                cx.notify();
                                            });
                                        }
                                    },
                                );
                            }
                            menu
                        })
                        .into()
                    })
                    .trigger(
                        ButtonLike::new("active-model")
                            .child(
                                h_flex()
                                    .w_full()
                                    .gap_0p5()
                                    .child(
                                        div()
                                            .overflow_x_hidden()
                                            .flex_grow()
                                            .whitespace_nowrap()
                                            .child(Label::new(self.model.clone())),
                                    )
                                    .child(div().child(
                                        Icon::new(IconName::ChevronDown).color(Color::Muted),
                                    )),
                            )
                            .style(ButtonStyle::Subtle)
                            .tooltip(move |cx| Tooltip::text("Change Model", cx)),
                    )
                    .anchor(gpui::AnchorCorner::TopRight),
            ),
        )
    }
}

impl Render for AssistantChat {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        div()
            .relative()
            .flex_1()
            .v_flex()
            .key_context("AssistantChat")
            .text_color(Color::Default.color(cx))
            .child(self.render_model_dropdown(cx))
            .child(list(self.list_state.clone()).flex_1())
    }
}

#[derive(Copy, Clone, Eq, PartialEq)]
struct MessageId(usize);

impl MessageId {
    fn post_inc(&mut self) -> Self {
        let id = *self;
        self.0 += 1;
        id
    }
}

enum ChatMessage {
    User(UserMessage),
    Assistant(AssistantMessage),
}

struct UserMessage {
    id: MessageId,
    body: View<Editor>,
    contexts: Vec<AssistantContext>,
}

// chain_of_thought: ... -> search -> search_results -> produce_new_message -> send for the real chat message

struct AssistantMessage {
    id: MessageId,
    body: RichText,
    error: Option<SharedString>,
}

enum AssistantContext {
    Codebase(CodebaseContext),
}

#[derive(Copy, Clone, Eq, PartialEq)]
struct ContextId(usize);

impl ContextId {
    fn post_inc(&mut self) -> Self {
        let id = *self;
        self.0 += 1;
        id
    }
}

struct CodebaseContext {
    id: ContextId,
    excerpts: Vec<CodebaseExcerpt>,
    pending: bool,
}

struct CodebaseExcerpt {
    path: SharedString,
    text: SharedString,
    score: f32,
}

impl AssistantContext {
    fn codebase(id: ContextId) -> Self {
        Self::Codebase(CodebaseContext {
            id,
            excerpts: Vec::new(),
            pending: true,
        })
    }

    fn render(&self, cx: &mut ViewContext<AssistantChat>) -> AnyElement {
        match self {
            AssistantContext::Codebase(context) => context.render(cx).into_any_element(),
        }
    }
}

impl CodebaseContext {
    fn render(&self, cx: &mut ViewContext<AssistantChat>) -> impl IntoElement {
        if self.pending {
            div().child("⏳")
        } else {
            div().children(self.excerpts.iter().map(|result| {
                div()
                    .p_2()
                    .rounded_md()
                    .bg(cx.theme().colors().editor_background)
                    .child(result.path.clone())
                    .child(result.text.clone())
            }))
        }
    }

    fn populate(&mut self, excerpts: Vec<CodebaseExcerpt>) {
        self.excerpts = excerpts;
        self.pending = false;
    }
}