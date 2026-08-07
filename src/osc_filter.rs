#[derive(Default, Debug, Clone, PartialEq, Eq)]
enum State {
    #[default]
    Normal,
    Escape,     // Saw '\x1b'
    OscPrefix,  // Saw '\x1b]'
    InOsc,      // Confirmed OSC 10/11, discarding until BEL (\x07) or ST (\x1b\)
    OscSeenEsc, // Saw '\x1b' while inside InOsc (checking for '\' to complete ST)
    CsiPrefix,  // Saw '\x1b['
    InDa,       // Confirmed DA1/DA2 (\x1b[? or \x1b[>), discarding until 'c'
}

#[derive(Default)]
pub struct OscFilter {
    state: State,
    pending: Vec<u8>, // Holds prefix bytes until we know if it's a target escape sequence
}

impl OscFilter {
    /// Filters OSC 10/11 and DA1/DA2 escape sequences from the byte stream.
    /// Preserves state across multiple `filter` calls for chunked streaming.
    pub fn filter(&mut self, input: &[u8]) -> Vec<u8> {
        let mut output = Vec::with_capacity(input.len());

        for &b in input {
            match self.state {
                State::Normal => {
                    if b == 0x1b {
                        // ESC
                        self.state = State::Escape;
                        self.pending.push(b);
                    } else {
                        output.push(b);
                    }
                }

                State::Escape => {
                    self.pending.push(b);
                    match b {
                        b']' => self.state = State::OscPrefix,
                        b'[' => self.state = State::CsiPrefix,
                        _ => {
                            // Not an OSC or CSI sequence, flush pending bytes & return to Normal
                            output.append(&mut self.pending);
                            self.state = State::Normal;
                        }
                    }
                }

                State::OscPrefix => {
                    self.pending.push(b);

                    // Check if we matched "10;" or "11;" after '\x1b]'
                    if self.pending.ends_with(b"10;") || self.pending.ends_with(b"11;") {
                        self.pending.clear();
                        self.state = State::InOsc;
                    } else if self.pending.len() >= 6 {
                        // Not an OSC 10/11 query, flush pending bytes & return to Normal
                        output.append(&mut self.pending);
                        self.state = State::Normal;
                    }
                }

                State::InOsc => {
                    if b == 0x07 {
                        // BEL terminates OSC
                        self.state = State::Normal;
                    } else if b == 0x1b {
                        // ESC might be start of ST (\x1b\)
                        self.state = State::OscSeenEsc;
                    }
                    // All other OSC bytes are discarded
                }

                State::OscSeenEsc => {
                    if b == b'\\' {
                        // ST (\x1b\) terminates OSC
                        self.state = State::Normal;
                    } else if b == 0x07 {
                        // BEL terminates OSC
                        self.state = State::Normal;
                    } else {
                        // False alarm, still in OSC
                        self.state = State::InOsc;
                    }
                }

                State::CsiPrefix => {
                    self.pending.push(b);
                    if b == b'?' || b == b'>' {
                        // DA1 (\x1b[?) or DA2 (\x1b[>)
                        self.pending.clear();
                        self.state = State::InDa;
                    } else {
                        // Not a DA query, flush pending bytes & return to Normal
                        output.append(&mut self.pending);
                        self.state = State::Normal;
                    }
                }

                State::InDa => {
                    if b == b'c' {
                        // 'c' terminates DA1/DA2 queries
                        self.state = State::Normal;
                    }
                    // All other DA bytes are discarded
                }
            }
        }

        output
    }
}
