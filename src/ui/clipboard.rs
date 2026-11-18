use std::{
    io::{self, Write},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub(super) fn copy(text: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        write("pbcopy", &[], text.as_bytes()).is_ok()
    }
    #[cfg(windows)]
    {
        let utf16: Vec<_> = std::iter::once(0xfeff)
            .chain(text.encode_utf16())
            .flat_map(u16::to_le_bytes)
            .collect();
        write("clip.exe", &[], &utf16).is_ok()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        (std::env::var_os("WAYLAND_DISPLAY").is_some()
            && write("wl-copy", &[], text.as_bytes()).is_ok())
            || (std::env::var_os("DISPLAY").is_some()
                && (write("xclip", &["-selection", "clipboard"], text.as_bytes()).is_ok()
                    || write("xsel", &["--clipboard", "--input"], text.as_bytes()).is_ok()))
    }
}

fn write(program: &str, args: &[&str], text: &[u8]) -> io::Result<()> {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let written = child.stdin.take().unwrap().write_all(text);
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if let Some(status) = child.try_wait()? {
            return written.and_then(|()| {
                if status.success() {
                    Ok(())
                } else {
                    Err(io::Error::other("clipboard command failed"))
                }
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::ErrorKind::TimedOut.into());
        }
        thread::sleep(Duration::from_millis(10));
    }
}
