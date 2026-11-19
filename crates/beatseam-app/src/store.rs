use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

type Listener<S> = Rc<dyn Fn(&S)>;
type Change<S> = Box<dyn FnOnce(&mut S)>;

/// The app's single observable state, the Redux store's counterpart: one value, changed only through [`dispatch`](Store::dispatch)
/// (a closure that plays the part of a reducer call), with subscribers told after every change. Where the TypeScript matched
/// action objects against a reducer table, a change here is just a call to a method on a slice (`state.settings.updated(patch)`),
/// which keeps the same properties - changes are small, named and unit-testable, and nothing but a dispatch ever mutates state -
/// without an action enum per slice.
///
/// Single-threaded by design (a UI's main thread). A dispatch made from inside a subscriber is queued and applied right after
/// the current one finishes, never nested.
pub struct Store<S> {
    state: RefCell<S>,
    listeners: RefCell<Vec<(u64, Listener<S>)>>,
    next_listener: Cell<u64>,
    queue: RefCell<VecDeque<Change<S>>>,
    dispatching: Cell<bool>,
}

impl<S: 'static> Store<S> {
    pub fn new(state: S) -> Self {
        Self { state: RefCell::new(state), listeners: RefCell::new(Vec::new()), next_listener: Cell::new(0), queue: RefCell::new(VecDeque::new()), dispatching: Cell::new(false) }
    }

    /// Reads (never clones the whole state): pass a closure that picks out what you need.
    pub fn read<R>(&self, f: impl FnOnce(&S) -> R) -> R {
        f(&self.state.borrow())
    }

    /// Applies a change and tells every subscriber.
    pub fn dispatch(&self, change: impl FnOnce(&mut S) + 'static) {
        self.queue.borrow_mut().push_back(Box::new(change));
        if self.dispatching.replace(true) {
            return; // an outer dispatch is notifying: it will pick this up
        }
        loop {
            let next = self.queue.borrow_mut().pop_front();
            let Some(change) = next else { break };
            change(&mut self.state.borrow_mut());
            let listeners: Vec<Listener<S>> = self.listeners.borrow().iter().map(|(_, l)| l.clone()).collect();
            let state = self.state.borrow();
            for listener in listeners {
                listener(&state);
            }
        }
        self.dispatching.set(false);
    }

    /// Calls `listener` after every change. Returns an id for [`unsubscribe`](Self::unsubscribe).
    pub fn subscribe(&self, listener: impl Fn(&S) + 'static) -> u64 {
        let id = self.next_listener.get() + 1;
        self.next_listener.set(id);
        self.listeners.borrow_mut().push((id, Rc::new(listener)));
        id
    }

    pub fn unsubscribe(&self, id: u64) {
        self.listeners.borrow_mut().retain(|(i, _)| *i != id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_changes_state_and_notifies_every_subscriber_until_they_unsubscribe() {
        let store = Store::new(0);
        let seen = Rc::new(RefCell::new(Vec::new()));
        let s = seen.clone();
        let id = store.subscribe(move |v| s.borrow_mut().push(*v));
        store.dispatch(|v| *v += 1);
        store.dispatch(|v| *v += 10);
        assert_eq!(store.read(|v| *v), 11);
        store.unsubscribe(id);
        store.dispatch(|v| *v += 100);
        assert_eq!(*seen.borrow(), [1, 11]);
    }

    #[test]
    fn a_dispatch_from_inside_a_subscriber_is_queued_not_nested() {
        let store = Rc::new(Store::new(0));
        let order = Rc::new(RefCell::new(Vec::new()));
        let (st, o) = (store.clone(), order.clone());
        store.subscribe(move |v| {
            o.borrow_mut().push(format!("saw {v}"));
            if *v == 1 {
                st.dispatch(|v| *v = 2);
                o.borrow_mut().push("after nested dispatch call".into());
            }
        });
        store.dispatch(|v| *v = 1);
        assert_eq!(*order.borrow(), ["saw 1", "after nested dispatch call", "saw 2"]);
        assert_eq!(store.read(|v| *v), 2);
    }

    #[test]
    fn reading_inside_a_subscriber_is_fine() {
        let store = Rc::new(Store::new(5));
        let (st, got) = (store.clone(), Rc::new(Cell::new(0)));
        let g = got.clone();
        store.subscribe(move |_| g.set(st.read(|v| *v)));
        store.dispatch(|v| *v = 9);
        assert_eq!(got.get(), 9);
    }
}
