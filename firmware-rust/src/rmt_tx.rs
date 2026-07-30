//! Fair, process-wide arbitration for the shared ESP32 RMT TX resource.
//!
//! ESP32-C5 exposes 48 RMT symbols per physical channel. Requesting a larger
//! block makes ESP-IDF reserve an adjacent channel as backing memory, which can
//! leave no TX channel for another peripheral. Status LED and IR transmissions
//! therefore use one FIFO lease and create their hardware drivers only while
//! that lease is held.

use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};

/// Native RMT memory available to one ESP32-C5 channel.
pub const ESP32_C5_RMT_SYMBOLS_PER_CHANNEL: usize = 48;

/// C5 value kept explicit so host tests can enforce the one-channel limit.
pub const ESP32_C5_SHARED_RMT_TX_MEMORY_BLOCK_SYMBOLS: usize = 48;

const _: () =
    assert!(ESP32_C5_SHARED_RMT_TX_MEMORY_BLOCK_SYMBOLS <= ESP32_C5_RMT_SYMBOLS_PER_CHANNEL);

/// Memory block used by drivers participating in the shared TX lease. Classic
/// ESP32/WROOM requires at least 64 symbols; C5 has a native 48-symbol block.
#[cfg(aethersense_esp32c5)]
pub const SHARED_RMT_TX_MEMORY_BLOCK_SYMBOLS: usize = ESP32_C5_SHARED_RMT_TX_MEMORY_BLOCK_SYMBOLS;
#[cfg(not(aethersense_esp32c5))]
pub const SHARED_RMT_TX_MEMORY_BLOCK_SYMBOLS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RmtTxClient {
    StatusLed,
    Infrared,
}

#[derive(Debug, Default)]
struct ArbiterState {
    next_ticket: u64,
    serving_ticket: u64,
    owner: Option<RmtTxClient>,
    waiters: usize,
}

/// FIFO mutex for peripherals which temporarily allocate an RMT TX channel.
#[derive(Debug, Default)]
pub struct RmtTxArbiter {
    state: Mutex<ArbiterState>,
    changed: Condvar,
}

impl RmtTxArbiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Waits for this client's turn. The returned lease releases the resource
    /// on drop, including early-return and unwind paths.
    pub fn acquire(&self, client: RmtTxClient) -> RmtTxLease<'_> {
        let mut state = self.lock_state();
        let ticket = state.next_ticket;
        state.next_ticket = state.next_ticket.wrapping_add(1);
        state.waiters += 1;

        while state.owner.is_some() || state.serving_ticket != ticket {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }

        state.waiters -= 1;
        state.owner = Some(client);
        drop(state);
        RmtTxLease {
            arbiter: self,
            client,
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, ArbiterState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[cfg(test)]
    fn waiting_count(&self) -> usize {
        self.lock_state().waiters
    }
}

/// RAII proof that one client exclusively owns the shared RMT TX allocation.
#[derive(Debug)]
pub struct RmtTxLease<'a> {
    arbiter: &'a RmtTxArbiter,
    client: RmtTxClient,
}

impl Drop for RmtTxLease<'_> {
    fn drop(&mut self) {
        let mut state = self.arbiter.lock_state();
        debug_assert_eq!(state.owner, Some(self.client));
        state.owner = None;
        state.serving_ticket = state.serving_ticket.wrapping_add(1);
        self.arbiter.changed.notify_all();
    }
}

pub fn shared_rmt_tx() -> &'static RmtTxArbiter {
    static ARBITER: OnceLock<RmtTxArbiter> = OnceLock::new();
    ARBITER.get_or_init(RmtTxArbiter::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    fn wait_for_waiters(arbiter: &RmtTxArbiter, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while arbiter.waiting_count() != expected {
            assert!(Instant::now() < deadline, "RMT TX waiter did not enqueue");
            thread::yield_now();
        }
    }

    #[test]
    fn memory_block_never_invades_the_second_c5_tx_channel() {
        assert_eq!(ESP32_C5_SHARED_RMT_TX_MEMORY_BLOCK_SYMBOLS, 48);
        assert!(ESP32_C5_SHARED_RMT_TX_MEMORY_BLOCK_SYMBOLS <= ESP32_C5_RMT_SYMBOLS_PER_CHANNEL);
    }

    #[test]
    fn lease_blocks_competitors_and_releases_on_drop() {
        let arbiter = Arc::new(RmtTxArbiter::new());
        let first = arbiter.acquire(RmtTxClient::StatusLed);
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let contender_arbiter = arbiter.clone();
        let contender = thread::spawn(move || {
            let _lease = contender_arbiter.acquire(RmtTxClient::Infrared);
            acquired_tx.send(()).unwrap();
        });

        wait_for_waiters(&arbiter, 1);
        assert!(matches!(
            acquired_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        drop(first);
        acquired_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        contender.join().unwrap();
    }

    #[test]
    fn queued_clients_acquire_in_ticket_order() {
        let arbiter = Arc::new(RmtTxArbiter::new());
        let first = arbiter.acquire(RmtTxClient::Infrared);
        let order = Arc::new(Mutex::new(Vec::new()));

        let first_waiter_arbiter = arbiter.clone();
        let first_waiter_order = order.clone();
        let first_waiter = thread::spawn(move || {
            let _lease = first_waiter_arbiter.acquire(RmtTxClient::StatusLed);
            first_waiter_order
                .lock()
                .unwrap()
                .push(RmtTxClient::StatusLed);
        });
        wait_for_waiters(&arbiter, 1);

        let second_waiter_arbiter = arbiter.clone();
        let second_waiter_order = order.clone();
        let second_waiter = thread::spawn(move || {
            let _lease = second_waiter_arbiter.acquire(RmtTxClient::Infrared);
            second_waiter_order
                .lock()
                .unwrap()
                .push(RmtTxClient::Infrared);
        });
        wait_for_waiters(&arbiter, 2);

        drop(first);
        first_waiter.join().unwrap();
        second_waiter.join().unwrap();
        assert_eq!(
            *order.lock().unwrap(),
            vec![RmtTxClient::StatusLed, RmtTxClient::Infrared]
        );
    }
}
