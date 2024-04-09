use anyhow::Context;
use regex::Regex;

use crate::{
    config::{self, FilterMode},
    ChannelIdentifier,
};
use std::{collections::HashMap, str::FromStr};

pub struct MessageRouter {
    pub channel_links: HashMap<ChannelIdentifier, Vec<MirroredChannel>>,
}

impl MessageRouter {
    pub fn new(config: &[config::Bridge]) -> anyhow::Result<Self> {
        let mut channel_links: HashMap<ChannelIdentifier, Vec<MirroredChannel>> = HashMap::new();

        for bridge_config in config {
            let [source, target] = &bridge_config.channels;
            let bidirectional = bridge_config.bidirectional.unwrap_or(true);
            let insert_zws = bridge_config.insert_zws_into_names.unwrap_or(false);

            let exclude_filters: Vec<Regex> = bridge_config
                .exclude_filters
                .iter()
                .map(|filter| Regex::new(filter).context("Invalid regex"))
                .collect::<anyhow::Result<_>>()?;

            let source_channel = ChannelIdentifier::from_str(source).unwrap();
            let target_channel = ChannelIdentifier::from_str(target).unwrap();

            channel_links
                .entry(source_channel.clone())
                .or_default()
                .push(MirroredChannel {
                    channel: target_channel.clone(),
                    insert_zws,
                    exclude_filters: exclude_filters.clone(),
                    filter_mode: bridge_config.filter_mode,
                });

            if bidirectional {
                channel_links
                    .entry(target_channel)
                    .or_default()
                    .push(MirroredChannel {
                        channel: source_channel,
                        insert_zws,
                        exclude_filters,
                        filter_mode: bridge_config.filter_mode,
                    });
            }
        }

        Ok(Self { channel_links })
    }
}

#[derive(Clone, Debug)]
pub struct MirroredChannel {
    pub channel: ChannelIdentifier,
    pub insert_zws: bool,
    pub exclude_filters: Vec<Regex>,
    pub filter_mode: FilterMode,
}
