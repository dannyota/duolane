use std::{
    collections::HashMap,
    io::{self, Write},
};

pub(super) fn supported(env: &HashMap<String, String>) -> bool {
    let program = env.get("TERM_PROGRAM").map(String::as_str).unwrap_or("");
    let term = env.get("TERM").map(String::as_str).unwrap_or("");
    if program == "tmux" || term.starts_with("tmux") || term.starts_with("screen") {
        return false;
    }
    matches!(program, "ghostty" | "WezTerm")
        || matches!(
            term,
            "xterm-ghostty" | "xterm-kitty" | "foot" | "foot-extra"
        )
}

#[derive(Default)]
pub(super) struct Pointer {
    enabled: bool,
    resize: bool,
}

impl Pointer {
    pub(super) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            resize: false,
        }
    }

    pub(super) fn update(&mut self, writer: &mut impl Write, resize: bool) -> io::Result<()> {
        if self.enabled && self.resize != resize {
            self.resize = resize;
            writer.write_all(if resize {
                b"\x1b]22;ns-resize\x1b\\"
            } else {
                b"\x1b]22;default\x1b\\"
            })?;
            writer.flush()?;
        }
        Ok(())
    }

    pub(super) fn reset(&mut self, writer: &mut impl Write) -> io::Result<()> {
        self.update(writer, false)
    }
}
