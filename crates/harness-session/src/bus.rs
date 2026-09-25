use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::events::HarnessEvent;

pub trait EventSubscriber: Send + Sync {
    fn on_event(&self, event: &HarnessEvent);
}

impl<F> EventSubscriber for F
where
    F: Fn(&HarnessEvent) + Send + Sync,
{
    fn on_event(&self, event: &HarnessEvent) {
        self(event);
    }
}

#[derive(Default)]
struct EventBusState {
    next_id: AtomicU64,
    subscribers: Mutex<Vec<(u64, Arc<dyn EventSubscriber>)>>,
}

#[derive(Clone, Default)]
pub struct EventBus {
    state: Arc<EventBusState>,
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("EventBus").finish()
    }
}

impl EventBus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn subscribe(&self, subscriber: Arc<dyn EventSubscriber>) -> EventSubscription {
        let id = self.state.next_id.fetch_add(1, Ordering::Relaxed);
        self.state
            .subscribers
            .lock()
            .expect("event bus lock poisoned")
            .push((id, subscriber));
        EventSubscription {
            bus: self.clone(),
            id,
        }
    }

    pub fn publish(&self, event: &HarnessEvent) {
        let subscribers = self
            .state
            .subscribers
            .lock()
            .expect("event bus lock poisoned")
            .iter()
            .map(|(_, subscriber)| Arc::clone(subscriber))
            .collect::<Vec<_>>();
        for subscriber in subscribers {
            subscriber.on_event(event);
        }
    }
}

pub struct EventSubscription {
    bus: EventBus,
    id: u64,
}

impl Drop for EventSubscription {
    fn drop(&mut self) {
        let mut subscribers = self
            .bus
            .state
            .subscribers
            .lock()
            .expect("event bus lock poisoned");
        subscribers.retain(|(id, _)| *id != self.id);
    }
}
