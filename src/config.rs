use serde::Deserialize;
use std::collections::HashMap;
use toml::Table;

#[derive(Deserialize, Clone)]
pub struct Config {
    pub general: General,
    #[serde(default)]
    pub platforms: Table,
    pub bridge: Vec<Bridge>,
    #[serde(default)]
    pub message: Message,
}

#[derive(Deserialize, Clone)]
pub struct General {
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default = "default_listen_address")]
    pub listen_address: String,
    pub base_url: String,
}

#[derive(Deserialize, Clone)]
pub struct Bridge {
    pub channels: [String; 2],
    pub bidirectional: Option<bool>,
    pub insert_zws_into_names: Option<bool>,
    #[serde(default)]
    pub exclude_filters: Vec<String>,
    #[serde(default)]
    pub filter_mode: FilterMode,
}

#[derive(Clone, Copy, Default, Deserialize, Debug)]
pub enum FilterMode {
    #[default]
    FinalMessage,
    SourceMessage,
}

fn default_log_level() -> String {
    "info".to_owned()
}

fn default_listen_address() -> String {
    "0.0.0.0:8000".to_owned()
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct Message {
    pub platform_aliases: HashMap<String, String>,
}
