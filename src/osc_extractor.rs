#[derive(Default, Debug, Clone, PartialEq, Eq)]
enum State {
    #[default]
    Normal,
    Escape,       // Saw '\x1b'
    OscPrefix,    // Saw '\x1b]' (matching prefix of "\x1b]52;")
    InOsc52,      // Confirmed OSC 52 sequence, buffering payload until BEL (\x07) or ST (\x1b\)
    Osc52SeenEsc, // Saw '\x1b' inside InOsc52 (checking for '\' or BEL to complete ST)
}

#[derive(Default)]
pub struct Osc52Extractor {
    state: State,
    buffer: Vec<u8>, // Holds sequence bytes until confirmed and fully completed
}

impl Osc52Extractor {
    /// Strips everything EXCEPT complete OSC 52 escape sequences from the byte stream.
    /// Buffers partial sequences across chunk boundaries to ensure atomic delivery
    /// to the local terminal (Alacritty) without being aborted by screen redraws.
    pub fn extract(&mut self, input: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();

        for &b in input {
            match self.state {
                State::Normal => {
                    if b == 0x1b {
                        self.state = State::Escape;
                        self.buffer.push(b);
                    }
                }

                State::Escape => {
                    self.buffer.push(b);
                    if b == b']' {
                        self.state = State::OscPrefix;
                    } else if b == 0x1b {
                        // Restart sequence if consecutive ESC bytes arrive
                        self.buffer.clear();
                        self.buffer.push(b);
                    } else {
                        // Not an OSC sequence, discard buffer
                        self.buffer.clear();
                        self.state = State::Normal;
                    }
                }

                State::OscPrefix => {
                    self.buffer.push(b);
                    let target = b"\x1b]52;";

                    if self.buffer == target {
                        self.state = State::InOsc52;
                    } else if target.starts_with(&self.buffer) {
                        // Still matching "\x1b]52;" prefix (e.g. "\x1b]5" or "\x1b]52")
                    } else {
                        // Not an OSC 52 sequence, discard buffer
                        self.buffer.clear();
                        if b == 0x1b {
                            self.state = State::Escape;
                            self.buffer.push(b);
                        } else {
                            self.state = State::Normal;
                        }
                    }
                }

                State::InOsc52 => {
                    self.buffer.push(b);
                    if b == 0x07 {
                        // BEL (\x07) terminates OSC 52
                        self.flush_complete_osc52(&mut output);
                        self.state = State::Normal;
                    } else if b == 0x1b {
                        // ESC might start String Terminator ST (\x1b\)
                        self.state = State::Osc52SeenEsc;
                    }
                }

                State::Osc52SeenEsc => {
                    self.buffer.push(b);
                    if b == b'\\' || b == 0x07 {
                        // ST (\x1b\) or BEL terminates OSC 52
                        self.flush_complete_osc52(&mut output);
                        self.state = State::Normal;
                    } else if b == b']' {
                        // New OSC sequence started; abort old sequence and start new prefix
                        self.buffer.clear();
                        self.buffer.push(0x1b);
                        self.buffer.push(b);
                        self.state = State::OscPrefix;
                    } else if b == b'[' {
                        // New CSI sequence started; abort old sequence
                        self.buffer.clear();
                        self.state = State::Normal;
                    } else if b == 0x1b {
                        // Consecutive ESC, stay in Osc52SeenEsc
                        self.state = State::Osc52SeenEsc;
                    } else {
                        // False alarm, resume payload capture
                        self.state = State::InOsc52;
                    }
                }
            }
        }

        output
    }

    fn flush_complete_osc52(&mut self, output: &mut Vec<u8>) {
        // Alacritty compatibility fix:
        // Convert "\x1b]52;;base64\x07" -> "\x1b]52;c;base64\x07" as Alacritty rejects empty targets.
        if self.buffer.starts_with(b"\x1b]52;;") {
            output.extend_from_slice(b"\x1b]52;c;");
            output.extend_from_slice(&self.buffer[6..]);
        } else {
            output.append(&mut self.buffer);
        }
        self.buffer.clear();
    }
}
