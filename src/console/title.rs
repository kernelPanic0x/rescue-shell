use std::io::{self, Write};

pub struct TitleSession {
    name: String,
}

impl TitleSession {
    /// Pushes the terminal title stack and sets the initial title
    pub fn new(name: String) -> Self {
        let mut stdout = io::stdout().lock();

        // Push title stack and set title
        let _ = write!(stdout, "\x1b[22;0t\x1b]0;rescue-shell");
        if !name.is_empty() {
            let _ = write!(stdout, " [{name}]");
        }
        let _ = write!(stdout, "\x07");
        let _ = stdout.flush();

        Self { name }
    }

    /// Dynamically update the title when running a sub-command
    pub fn set_command(&self, command: &str) {
        let mut stdout = io::stdout().lock();
        let _ = write!(stdout, "\x1b]0;rescue-shell");

        if !self.name.is_empty() {
            let _ = write!(stdout, " [{}]", self.name);
        }

        if !command.is_empty() {
            let _ = write!(stdout, ": {command}");
        }

        let _ = write!(stdout, "\x07");
        let _ = stdout.flush();
    }
}

// Automatically pops and restores the original terminal title on exit/panic
impl Drop for TitleSession {
    fn drop(&mut self) {
        let mut stdout = io::stdout().lock();
        let _ = write!(stdout, "\x1b[23;0t");
        let _ = stdout.flush();
    }
}
