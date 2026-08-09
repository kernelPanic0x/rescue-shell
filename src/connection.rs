use magic_wormhole::{AppConfig, Code, MailboxConnection, transfer::AppVersion};

use crate::{CommonArgs, app_config};

pub struct Connection {
    config: AppConfig<AppVersion>,
    mailbox: MailboxConnection<AppVersion>,
}

impl Connection {
    pub async fn new(common: CommonArgs, code: Option<Code>) -> anyhow::Result<Self> {
        let config = app_config(&common);

        let mailbox = match code {
            Some(code) => MailboxConnection::connect(config.clone(), code.clone(), false).await?,
            None => MailboxConnection::create(config.clone(), 2).await?,
        };

        Ok(Self { config, mailbox })
    }
}
