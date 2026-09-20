//! itan-core — Content-Addressable Hardlink & Storage Reclamation Engine
//!
//! # Architecture
//!
//! ```text
//! CapabilityProber → CasStore → WaterfallIngestion → AtomicLinkagePipeline
//!                                                           ↓
//!                                                    QuarantineGC + CrashRecovery
//! ```
//!
//! Each module enforces a strict single responsibility. No module holds global mutable state;
//! shared state is passed explicitly via `Arc<CasStore>` references.

pub mod capability;
pub mod digest;
pub mod gc;
pub mod ingest;
pub mod linkage;
pub mod permissions;
pub mod recovery;
pub mod retry_queue;
pub mod store;
pub mod whitelist;

#[cfg(all(target_os = "windows", feature = "windows-ntfs"))]
pub mod usn;

/// Re-export the primary public surface so consumers can `use itan_core::*;`.
pub use capability::{LinkStrategy, ProbeError, VolumeCapability};
pub use digest::DigestError;
pub use gc::{GcDecision, GcError, GcStats, QuarantineGc};
pub use ingest::{BypassReason, IngestError, IngestPipeline, IngestVerdict};
pub use linkage::{AtomicLinkagePipeline, LinkError, LinkResult};
pub use permissions::PermError;
pub use recovery::{RecoveryError, StartupSweeper, SweepReport};
pub use retry_queue::{DeferRetryQueue, RetryJob};
pub use store::{CasStore, SlotRef, StoreError};
pub use whitelist::{FileClass, classify_path};
