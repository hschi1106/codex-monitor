use std::{collections::BTreeMap, path::PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    pub name: String,
    pub executable: PathBuf,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
}
