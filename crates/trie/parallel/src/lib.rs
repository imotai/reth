//! Implementation of exotic state root computation approaches.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod storage_root_targets;
pub use storage_root_targets::StorageRootTargets;

/// Parallel trie calculation stats.
pub mod stats;

/// Implementation of parallel state root computation.
pub mod root;

/// Implementation of parallel proof computation.
pub mod proof;

pub mod proof_task;

/// Parallel state root metrics.
#[cfg(feature = "metrics")]
pub mod metrics;

/// Proof task manager metrics.
#[cfg(feature = "metrics")]
pub mod proof_task_metrics;
