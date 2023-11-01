use std::ops::{Deref, DerefMut};

use editor::test::{
    editor_lsp_test_context::EditorLspTestContext, editor_test_context::EditorTestContext,
};
use futures::Future;
use gpui::ContextHandle;
use lsp::request;
use search::{BufferSearchBar, ProjectSearchBar};

use crate::{state::Operator, *};

pub struct VimTestContext<'a> {
    cx: EditorLspTestContext<'a>,
}

impl<'a> VimTestContext<'a> {
    pub async fn new(cx: &'a mut gpui::TestAppContext, enabled: bool) -> VimTestContext<'a> {
        let lsp = EditorLspTestContext::new_rust(Default::default(), cx).await;
        Self::new_with_lsp(lsp, enabled)
    }

    pub async fn new_typescript(cx: &'a mut gpui::TestAppContext) -> VimTestContext<'a> {
        Self::new_with_lsp(
            EditorLspTestContext::new_typescript(Default::default(), cx).await,
            true,
        )
    }

    pub fn new_with_lsp(mut cx: EditorLspTestContext<'a>, enabled: bool) -> VimTestContext<'a> {
        cx.update(|cx| {
            search::init(cx);
            crate::init(cx);
            command_palette::init(cx);
        });

        cx.update(|cx| {
            cx.update_global(|store: &mut SettingsStore, cx| {
                store.update_user_settings::<VimModeSetting>(cx, |s| *s = Some(enabled));
            });
            settings::KeymapFile::load_asset("keymaps/default.json", cx).unwrap();
            settings::KeymapFile::load_asset("keymaps/vim.json", cx).unwrap();
        });

        // Setup search toolbars and keypress hook
        cx.update_workspace(|workspace, cx| {
            observe_keystrokes(cx);
            workspace.active_pane().update(cx, |pane, cx| {
                pane.toolbar().update(cx, |toolbar, cx| {
                    let buffer_search_bar = cx.add_view(BufferSearchBar::new);
                    toolbar.add_item(buffer_search_bar, cx);
                    let project_search_bar = cx.add_view(|_| ProjectSearchBar::new());
                    toolbar.add_item(project_search_bar, cx);
                })
            });
            workspace.status_bar().update(cx, |status_bar, cx| {
                let vim_mode_indicator = cx.add_view(ModeIndicator::new);
                status_bar.add_right_item(vim_mode_indicator, cx);
            });
        });

        Self { cx }
    }

    pub fn workspace<F, T>(&mut self, read: F) -> T
    where
        F: FnOnce(&Workspace, &ViewContext<Workspace>) -> T,
    {
        self.cx.workspace.read_with(self.cx.cx.cx, read)
    }

    pub fn enable_vim(&mut self) {
        self.cx.update(|cx| {
            cx.update_global(|store: &mut SettingsStore, cx| {
                store.update_user_settings::<VimModeSetting>(cx, |s| *s = Some(true));
            });
        })
    }

    pub fn disable_vim(&mut self) {
        self.cx.update(|cx| {
            cx.update_global(|store: &mut SettingsStore, cx| {
                store.update_user_settings::<VimModeSetting>(cx, |s| *s = Some(false));
            });
        })
    }

    pub fn mode(&mut self) -> Mode {
        self.cx.read(|cx| cx.global::<Vim>().state().mode)
    }

    pub fn active_operator(&mut self) -> Option<Operator> {
        self.cx
            .read(|cx| cx.global::<Vim>().state().operator_stack.last().copied())
    }

    pub fn set_state(&mut self, text: &str, mode: Mode) -> ContextHandle {
        let window = self.window;
        let context_handle = self.cx.set_state(text);
        window.update(self.cx.cx.cx, |cx| {
            Vim::update(cx, |vim, cx| {
                vim.switch_mode(mode, true, cx);
            })
        });
        self.cx.foreground().run_until_parked();
        context_handle
    }

    #[track_caller]
    pub fn assert_state(&mut self, text: &str, mode: Mode) {
        self.assert_editor_state(text);
        assert_eq!(self.mode(), mode, "{}", self.assertion_context());
    }

    pub fn assert_binding<const COUNT: usize>(
        &mut self,
        keystrokes: [&str; COUNT],
        initial_state: &str,
        initial_mode: Mode,
        state_after: &str,
        mode_after: Mode,
    ) {
        self.set_state(initial_state, initial_mode);
        self.cx.simulate_keystrokes(keystrokes);
        self.cx.assert_editor_state(state_after);
        assert_eq!(self.mode(), mode_after, "{}", self.assertion_context());
        assert_eq!(self.active_operator(), None, "{}", self.assertion_context());
    }

    pub fn handle_request<T, F, Fut>(
        &self,
        handler: F,
    ) -> futures::channel::mpsc::UnboundedReceiver<()>
    where
        T: 'static + request::Request,
        T::Params: 'static + Send,
        F: 'static + Send + FnMut(lsp::Url, T::Params, gpui::AsyncAppContext) -> Fut,
        Fut: 'static + Send + Future<Output = Result<T::Result>>,
    {
        self.cx.handle_request::<T, F, Fut>(handler)
    }
}

impl<'a> Deref for VimTestContext<'a> {
    type Target = EditorTestContext<'a>;

    fn deref(&self) -> &Self::Target {
        &self.cx
    }
}

impl<'a> DerefMut for VimTestContext<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.cx
    }
}
