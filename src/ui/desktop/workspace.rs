//! Workspace switching and asynchronous native folder-picker lifecycle.

use super::*;

impl DesktopApp {
    pub(super) fn activate_workspace(&mut self, path: PathBuf) {
        if self.workspace.selected_workspace.as_deref() == Some(path.as_path()) {
            return;
        }
        self.runtime.codex = CodexRuntimeController::spawn_with_web_browser(
            path.clone(),
            self.runtime.web_browser.clone(),
        );
        self.runtime.codex_connection = CodexConnection::Starting;
        self.runtime.codex_thread_id = None;
        self.runtime.codex_turn_id = None;
        self.runtime.codex_model = None;
        self.runtime.codex_catalog_source = None;
        self.runtime.codex_catalog.clear();
        self.workspace.selected_workspace = Some(path.clone());
        self.runtime_message = Some(format!("작업공간 전환: {}", path.display()));
    }

    pub(super) fn open_workspace_picker(&mut self) {
        if self.workspace.workspace_picker.is_some() {
            return;
        }
        let handle = std::thread::spawn(|| {
            rfd::FileDialog::new()
                .set_title("로컬 폴더 선택")
                .pick_folder()
                .map(|path| path.to_path_buf())
        });
        self.workspace.workspace_picker = Some(handle);
    }

    pub(super) fn drain_workspace_picker(&mut self) {
        let Some(handle) = self.workspace.workspace_picker.take() else {
            return;
        };
        if !handle.is_finished() {
            self.workspace.workspace_picker = Some(handle);
            return;
        }
        match handle.join() {
            Ok(Some(path)) => self.add_workspace(path),
            Ok(None) => {}
            Err(_) => self.runtime_message = Some("폴더 선택이 실패했습니다".to_owned()),
        }
    }
}
