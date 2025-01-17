//! STAR Randomness web service
//! Epoch and key state and its management

use std::sync::{Arc, RwLock};
use time::format_description::well_known::Rfc3339;
use tracing::{info, instrument};

use crate::Config;
use ppoprf::ppoprf;

/// Internal state of the OPRF service
pub struct OPRFServer {
    /// oprf implementation
    pub server: ppoprf::Server,
    /// currently-valid randomness epoch
    pub epoch: u8,
    /// RFC 3339 timestamp of the next epoch rotation
    pub next_epoch_time: Option<String>,
}

/// Shareable wrapper around the server state
pub type OPRFState = Arc<RwLock<OPRFServer>>;

impl OPRFServer {
    /// Initialize a new OPRFServer state with the given configuration
    pub fn new(config: &Config) -> Result<Self, ppoprf::PPRFError> {
        // ppoprf wants a vector, so generate one from our range.
        let epochs: Vec<u8> =
            (config.first_epoch..=config.last_epoch).collect();
        let epoch = epochs[0];
        let server = ppoprf::Server::new(epochs)?;
        Ok(OPRFServer {
            server,
            epoch,
            next_epoch_time: None,
        })
    }
}

/// Advance to the next epoch on a timer
/// This can be invoked as a background task to handle epoch
/// advance and key rotation according to the given Config.
#[instrument(skip_all)]
pub async fn epoch_loop(state: OPRFState, config: &Config) {
    let epochs = config.first_epoch..=config.last_epoch;

    let interval =
        std::time::Duration::from_secs(config.epoch_seconds.into());
    info!("rotating epoch every {} seconds", interval.as_secs());

    let start_time = time::OffsetDateTime::now_utc();
    // Epoch base_time comes from a config argument if given,
    // otherwise use start_time.
    let base_time = config.epoch_base_time.unwrap_or(start_time);
    info!(
        "epoch base time = {}",
        base_time
            .format(&Rfc3339)
            .expect("well-known timestamp format should always succeed")
    );

    // Calculate where we are in the epoch schedule relative to the
    // base time. We may need to start in the middle of the range.
    assert!(
        start_time >= base_time,
        "epoch-base-time should be in the past"
    );
    // The time difference will be positive after the assert.
    // The ratio of two Durations is an f64 (in seconds) which covers
    // the representable range of `OffsetDateTime`.
    let elapsed_epochs = ((start_time - base_time) / interval).floor() as u64;

    // The `epochs` range is `u8`, so the length can be no more
    // than `u8::MAX + 1`, making it safe to truncate the modulo.
    let offset = elapsed_epochs % epochs.len() as u64;
    let current_epoch = epochs.start() + offset as u8;

    // Advance to the current epoch if base time indicates we started
    // in the middle of a sequence.
    if current_epoch != config.first_epoch {
        info!(
            "Puncturing obsolete epochs {}..{} to match base time",
            config.first_epoch, current_epoch
        );
        let mut s = state.write().expect("Failed to lock OPRFState");
        for epoch in config.first_epoch..current_epoch {
            s.server
                .puncture(epoch)
                .expect("Failed to puncture obsolete epoch");
        }
        s.epoch = current_epoch;
        info!("epoch now {}", s.epoch);
    }

    // First rotation happens after whatever time remains for the current epoch.
    // `Duration` doesn't implement `Mul<u64>` so we must truncate the elapsed
    // epoch count. Assert that this is valid in case base_time is very large
    // while inverval is small.
    assert!(elapsed_epochs < u32::MAX as u64, "cast mustn't overflow");
    let mut next_rotation =
        base_time + interval * (elapsed_epochs + 1) as u32;

    loop {
        // Pre-calculate the next_epoch_time for the InfoResponse hander.
        // Truncate to the nearest second.
        let timestamp = next_rotation
            .replace_millisecond(0)
            .expect("should be able to truncate to a fixed ms")
            .format(&Rfc3339)
            .expect("well-known timestamp format should always succeed");
        {
            // Acquire a temporary write lock which should be dropped
            // before sleeping. The locking should not fail, but if it
            // does we can't set the field back to None, so panic rather
            // than report stale information.
            let mut s = state
                .write()
                .expect("should be able to update next_epoch_time");
            s.next_epoch_time = Some(timestamp);
        }

        // Wait until the current epoch ends.
        let sleep_duration = next_rotation - time::OffsetDateTime::now_utc();
        // Negative durations mean we're behind.
        if sleep_duration.is_positive() {
            tokio::time::sleep(sleep_duration.unsigned_abs()).await;
        }
        next_rotation += interval;

        // Acquire exclusive access to the oprf state.
        // Panics if this fails, since processing requests with an
        // expired epoch weakens user privacy.
        let mut s = state.write().expect("Failed to lock OPRFState");

        // Puncture the current epoch so it can no longer be used.
        let old_epoch = s.epoch;
        s.server
            .puncture(old_epoch)
            .expect("Failed to puncture current epoch");

        // Advance to the next epoch, checking for overflow
        // and out-of-range.
        let new_epoch = old_epoch.checked_add(1);
        if new_epoch.filter(|e| epochs.contains(e)).is_some() {
            // Server is already initialized for this one.
            s.epoch = new_epoch.unwrap();
        } else {
            info!("Epochs exhausted! Rotating OPRF key");
            // Panics if this fails. Puncture should mean we can't
            // violate privacy through further evaluations, but we
            // still want to drop the inner state with its private key.
            *s = OPRFServer::new(config)
                .expect("Could not initialize new PPOPRF state");
        }
        info!("epoch now {}", s.epoch);
    }
}
