// SPDX-License-Identifier: EUPL-1.2
// Copyright (c) 2026–present rescue-shell contributors

use base64::prelude::*;
use iroh::PublicKey;
use std::{
    io::{Read, Write, stdin, stdout},
    path::PathBuf,
    str::FromStr,
};

use crate::console::{OSC_RESCUE_SHELL_CMD, OSC_RESCUE_SHELL_NAME};

pub fn copy_to_osc52() -> color_eyre::Result<()> {
    let mut buffer = Vec::new();
    stdin().read_to_end(&mut buffer)?;

    let encoded = BASE64_STANDARD.encode(&buffer);

    let mut stdout = stdout().lock();
    write!(stdout, "\x1b]52;c;{encoded}\x07")?;
    stdout.flush()?;

    Ok(())
}

pub fn print_new_name(name: &str) -> color_eyre::Result<()> {
    let mut stdout = stdout().lock();
    write!(
        stdout,
        "\x1b]{OSC_RESCUE_SHELL_CMD};{OSC_RESCUE_SHELL_NAME};{name}\x07"
    )?;
    stdout.flush()?;
    Ok(())
}

pub fn read_public_keys_file(path: &PathBuf) -> color_eyre::Result<Vec<PublicKey>> {
    let contents = std::fs::read_to_string(path)?;

    let pub_keys = contents
        .split_whitespace()
        .map(|s| PublicKey::from_str(s.trim()))
        .collect::<Result<Vec<PublicKey>, _>>()?;

    Ok(pub_keys)
}
