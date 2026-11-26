use std::{
    collections::HashMap,
    io::{self, Write},
};

pub(super) fn supported(env: &HashMap<String, String>) -> bool {
    let program = env.get("TERM_PROGRAM").map(String::as_str).unwrap_or("");
    let term = env.get("TERM").map(String::as_str).unwrap_or("");
    if program == "tmux"
        || term.starts_with("tmux")
        || term.starts_with("screen")
        || env.contains_key("TMUX")
        || env.contains_key("STY")
    {
        return false;
    }
    program == "ghostty"
        || matches!(
            term,
            "xterm-ghostty" | "xterm-kitty" | "foot" | "foot-extra"
        )
}

#[derive(Default)]
pub(super) struct Pointer {
    enabled: bool,
    resize: Option<bool>,
    needs_reset: bool,
}

impl Pointer {
    pub(super) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            resize: Some(false),
            needs_reset: false,
        }
    }

    pub(super) fn update(&mut self, writer: &mut impl Write, resize: bool) -> io::Result<()> {
        if self.enabled && self.resize != Some(resize) {
            if resize {
                self.needs_reset = true;
            }
            self.resize = None;
            writer.write_all(if resize {
                b"\x1b]22;ns-resize\x1b\\"
            } else {
                b"\x1b]22;default\x1b\\"
            })?;
            writer.flush()?;
            self.resize = Some(resize);
            self.needs_reset = resize;
        }
        Ok(())
    }

    pub(super) fn reset(&mut self, writer: &mut impl Write) -> io::Result<()> {
        if self.needs_reset {
            self.update(writer, false)?;
        }
        Ok(())
    }
}

pub(super) fn restore<W: Write>(
    pointer: &mut Pointer,
    writer: &mut W,
    restore_terminal: impl FnOnce(&mut W),
) {
    let _ = pointer.reset(writer);
    restore_terminal(writer);
}
