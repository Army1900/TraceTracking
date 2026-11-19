use super::*;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub(super) struct DesktopActions {
    copy: Option<mpsc::Receiver<&'static str>>,
    open: Option<mpsc::Receiver<&'static str>>,
}

impl App {
    pub(super) fn copy_text(&mut self, text: String) {
        if self.desktop.copy.is_none() {
            let (send, receive) = mpsc::channel();
            self.desktop.copy = Some(receive);
            std::thread::spawn(move || {
                let message = if clipboard::copy(&text) {
                    "已复制到剪贴板"
                } else {
                    "无法写入剪贴板"
                };
                let _ = send.send(message);
            });
        }
        self.notice = Some("复制中…");
    }

    pub(super) fn open_file(&mut self, path: PathBuf) {
        let (send, receive) = mpsc::channel();
        self.desktop.open = Some(receive);
        self.notice = Some("正在打开文件…");
        std::thread::spawn(move || {
            let message = match path.metadata() {
                Ok(meta) if meta.is_file() => {
                    if open_default(&path) {
                        "已交给系统默认应用打开"
                    } else {
                        "系统无法打开此文件"
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => "文件已不存在",
                Ok(_) => "此路径当前不是文件",
                Err(_) => "无法访问文件",
            };
            let _ = send.send(message);
        });
    }

    pub(super) fn clipboard_pending(&self) -> bool {
        self.desktop.copy.is_some()
    }

    pub(super) fn poll_desktop(&mut self) {
        for pending in [&mut self.desktop.open, &mut self.desktop.copy] {
            let Some(result) = pending else { continue };
            let message = match result.try_recv() {
                Ok(message) => message,
                Err(mpsc::TryRecvError::Empty) => continue,
                Err(mpsc::TryRecvError::Disconnected) => "系统操作失败",
            };
            *pending = None;
            self.notice = Some(message);
            self.dirty = true;
        }
    }
}

#[cfg(not(windows))]
fn open_default(path: &Path) -> bool {
    use std::process::{Command, Stdio};

    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    Command::new(program)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(windows)]
fn open_default(path: &Path) -> bool {
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::{
        System::Com::{
            COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
        },
        UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
    };

    let path = path.to_string_lossy();
    // The store canonicalizes Windows roots with a verbatim prefix. Shell
    // associations expect the usual drive or UNC spelling.
    let path = if let Some(path) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{path}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(&path).to_owned()
    };
    let path: Vec<_> = path.encode_utf16().chain(std::iter::once(0)).collect();
    // The worker owns COM initialization and the NUL-terminated filename
    // remains live for the ShellExecuteW call.
    unsafe {
        let hr = CoInitializeEx(
            null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        );
        let result = ShellExecuteW(
            null_mut(),
            null(),
            path.as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        );
        if hr >= 0 {
            CoUninitialize();
        }
        result as isize > 32
    }
}
