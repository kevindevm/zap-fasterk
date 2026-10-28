//! Self-update from GitHub releases, through fastframe-update.
//!
//! The crate checks for a newer release, refuses package-managed copies,
//! downloads and verifies the update against the publisher signature, and
//! hands it to a helper that installs it and rolls back if it does not start.
//! Zap Fasterk keeps its names, its key, its proxy and its interface.

pub use fastframe_update::{
    CHECK_INTERVAL, DownloadState, Installation, Kind, Prepared, Release, Source, Unsupported,
    Updater,
};
use fastframe_update::{MacConfig, ReqwestTransport, UpdateConfig};

/// Zap Fasterk's releases and the names its installations have had.
pub const CONFIG: UpdateConfig = UpdateConfig {
    // Upstream installations are never treated as Zap Fasterk update targets.
    legacy_names: &[],
    macos: MacConfig {
        bundle_ids: &["io.github.kevindevm.ZapFasterk"],
        executable_names: &[],
        legacy_bundle_names: &[],
    },
    publisher_key: Some(include_str!("../assets/update-public-key.hex")),
    // Only Zap Fasterk signatures authorize updates.
    additional_publisher_keys: &[],
    ..UpdateConfig::new(
        "kevindevm/zap-fasterk",
        "Zap Fasterk",
        "zap-fasterk",
        env!("CARGO_PKG_VERSION"),
    )
};

/// An updater on the proxy-aware reqwest client.
pub fn updater() -> anyhow::Result<Updater> {
    let mut builder = reqwest::blocking::Client::builder();
    if let Some(proxy) = crate::proxy::reqwest_proxy() {
        builder = builder.proxy(proxy);
    }
    Ok(Updater::new(CONFIG, ReqwestTransport::new(builder)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_config_is_valid() {
        CONFIG.validate().unwrap();
        assert_eq!(CONFIG.current_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(CONFIG.slug, "zap-fasterk");
        assert_eq!(CONFIG.repository, "kevindevm/zap-fasterk");
        assert!(CONFIG.additional_publisher_keys.is_empty());
        assert!(CONFIG.legacy_names.is_empty());
    }

    #[test]
    fn the_updater_starts_on_github() {
        assert!(updater().unwrap().source().is_github());
    }
}
