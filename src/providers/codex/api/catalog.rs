use std::{
    collections::BTreeSet,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::time::Instant;

use super::super::translate::{
    model_allowlist::{ALLOWED_MODELS, FAMILY_ALIASES, version},
    model_catalog::valid_slug,
};

#[derive(Default)]
pub(super) struct Catalog {
    pub(super) attempt: tokio::sync::Mutex<Option<Instant>>,
    pub(super) snapshot: RwLock<Option<Arc<[String]>>>,
}

impl Catalog {
    pub(super) fn snapshot(&self) -> Option<Arc<[String]>> {
        self.snapshot.read().ok()?.clone()
    }

    pub(super) fn replace(&self, bytes: &[u8]) -> Option<()> {
        let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
        let data = value.get("data")?.as_array()?;
        if data.len() > 4096 {
            return None;
        }
        let ids = data
            .iter()
            .map(|item| item.get("id")?.as_str())
            .collect::<Option<Vec<_>>>()?;
        let ids: BTreeSet<String> = ids
            .into_iter()
            .filter(|id| {
                valid_slug(id)
                    && (ALLOWED_MODELS.contains(id)
                        || FAMILY_ALIASES
                            .iter()
                            .any(|family| version(id, family).is_some()))
            })
            .map(str::to_owned)
            .collect();
        *self.snapshot.write().ok()? = Some(ids.into_iter().collect::<Vec<_>>().into());
        Some(())
    }
}

pub(super) const REFRESH_INTERVAL: Duration = Duration::from_secs(3600);
