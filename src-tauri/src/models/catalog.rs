//! Embedded model catalog. Downloadable artifacts are pinned by URL and
//! SHA-256; nothing is ever fetched that isn't listed here.

use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelEntry {
    pub id: String,
    pub display_name: String,
    pub tier: String,
    pub description: String,
    pub engine: String,
    pub dir_name: String,
    pub url: String,
    pub sha256: String,
    pub disk_bytes: u64,
    pub est_ram_bytes: u64,
    pub wer_pct: f32,
    pub native_punct: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PunctuationEntry {
    pub dir_name: String,
    pub url: String,
    pub sha256: String,
    pub disk_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolishEntry {
    pub file_name: String,
    pub url: String,
    pub sha256: String,
    pub disk_bytes: u64,
    pub est_ram_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    pub models: Vec<ModelEntry>,
    pub punctuation: PunctuationEntry,
    pub ai_polish: PolishEntry,
}

pub fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        // Windows editors often save UTF-8 with a BOM, which serde_json rejects.
        let raw = include_str!("../../resources/model_catalog.json").trim_start_matches('\u{feff}');
        serde_json::from_str(raw).expect("model_catalog.json is valid")
    })
}

pub fn entry(id: &str) -> Option<&'static ModelEntry> {
    catalog().models.iter().find(|m| m.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every artifact comes over HTTPS with a full SHA-256, written in the
    /// lower-case hex the downloader compares against. The Hugging Face file
    /// is named by commit rather than by branch, so the file behind the URL
    /// cannot change while the checksum stays the same.
    #[test]
    fn every_download_is_checksummed_and_the_model_url_names_a_commit() {
        let c = catalog();
        let mut all: Vec<(&str, &str)> =
            c.models.iter().map(|m| (m.url.as_str(), m.sha256.as_str())).collect();
        all.push((&c.punctuation.url, &c.punctuation.sha256));
        all.push((&c.ai_polish.url, &c.ai_polish.sha256));
        for (url, sha) in all {
            assert!(url.starts_with("https://"), "{url}");
            assert!(
                sha.len() == 64 && sha.chars().all(|ch| matches!(ch, '0'..='9' | 'a'..='f')),
                "{url}: {sha}"
            );
        }
        let revision = c
            .ai_polish
            .url
            .split("/resolve/")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .expect("a Hugging Face resolve URL");
        assert!(
            revision.len() == 40 && revision.chars().all(|ch| matches!(ch, '0'..='9' | 'a'..='f')),
            "the model URL names {revision:?}, not a commit"
        );
    }
}
