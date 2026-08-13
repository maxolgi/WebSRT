//! No-op QUIC congestion controller. Returns an unlimited window so quinn
//! never throttles or paces datagram sends — making QUIC behave like raw UDP.
//! SRT owns all congestion control and reliability.

use std::any::Any;
use std::sync::Arc;
use std::time::Instant;
use wtransport::quinn::congestion::{Controller, ControllerFactory};

#[derive(Clone, Default)]
struct Unlimited;

impl Controller for Unlimited {
    fn on_congestion_event(
        &mut self,
        _now: Instant,
        _sent: Instant,
        _is_persistent_congestion: bool,
        _lost_bytes: u64,
    ) {
    }

    fn on_mtu_update(&mut self, _new_mtu: u16) {}

    fn window(&self) -> u64 {
        u64::MAX
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        u64::MAX
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

#[derive(Default)]
pub struct UnlimitedFactory;

impl ControllerFactory for UnlimitedFactory {
    fn build(self: Arc<Self>, _now: Instant, _current_mtu: u16) -> Box<dyn Controller> {
        Box::new(Unlimited)
    }
}
