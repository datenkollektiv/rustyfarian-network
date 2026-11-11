//! Admission rule for subscriptions added while the client runs (ADR 023, rung 1).
//!
//! A tier keeps the builder's subscriptions as a fixed list and the runtime ones
//! as a bounded list of its own, with its own storage.
//! Both lists are re-issued after every connect that needs them, builder entries
//! first, then runtime entries in registration order (see
//! [`ConnectWorkPlan`](super::ConnectWorkPlan)).
//! [`admit_subscription`] is the one decision about a new runtime request.

use super::QoS;

/// What a tier does with a runtime `subscribe(topic, qos)` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionAdmission {
    /// Append the topic to the runtime list and subscribe on the live session.
    Added,
    /// The runtime entry at this index exists with another QoS: store the new QoS
    /// and subscribe again on the live session.
    Changed(usize),
    /// The topic is already subscribed at this QoS: nothing to do, report success.
    Unchanged,
    /// The runtime list already holds `capacity` entries.
    Full,
}

/// Decides how a runtime subscription request is handled.
///
/// `fixed` are the builder's subscriptions, `runtime` the ones added so far, both
/// as `(filter, qos)` in list order; `capacity` bounds the runtime list.
/// The rules, in order:
///
/// 1. A runtime entry with the same filter: [`Unchanged`](SubscriptionAdmission::Unchanged)
///    for the same QoS, [`Changed`](SubscriptionAdmission::Changed) otherwise.
/// 2. A builder entry with the same filter and QoS: `Unchanged`.
///    A builder entry with another QoS cannot be edited, so the request is
///    appended to the runtime list; being later in the list, it wins after a reconnect.
/// 3. Otherwise [`Added`](SubscriptionAdmission::Added), or
///    [`Full`](SubscriptionAdmission::Full) when the runtime list has no room.
///
/// Filters compare as exact strings; the caller validates the filter and clamps the QoS first.
///
/// ```
/// use juggler::mqtt::{admit_subscription, QoS, SubscriptionAdmission};
///
/// let fixed = [("cmd/#", QoS::AtLeastOnce)];
/// let runtime: [(&str, QoS); 0] = [];
/// assert_eq!(
///     admit_subscription(fixed, runtime, "cmd/#", QoS::AtLeastOnce, 4),
///     SubscriptionAdmission::Unchanged
/// );
/// assert_eq!(
///     admit_subscription(fixed, runtime, "ota/#", QoS::AtLeastOnce, 4),
///     SubscriptionAdmission::Added
/// );
/// ```
pub fn admit_subscription<'a>(
    fixed: impl IntoIterator<Item = (&'a str, QoS)>,
    runtime: impl IntoIterator<Item = (&'a str, QoS)>,
    topic: &str,
    qos: QoS,
    capacity: usize,
) -> SubscriptionAdmission {
    let mut runtime_len = 0;
    for (index, (filter, existing)) in runtime.into_iter().enumerate() {
        runtime_len = index + 1;
        if filter == topic {
            return if existing == qos {
                SubscriptionAdmission::Unchanged
            } else {
                SubscriptionAdmission::Changed(index)
            };
        }
    }
    if fixed
        .into_iter()
        .any(|(filter, existing)| filter == topic && existing == qos)
    {
        return SubscriptionAdmission::Unchanged;
    }
    if runtime_len >= capacity {
        SubscriptionAdmission::Full
    } else {
        SubscriptionAdmission::Added
    }
}

/// What the tracker did with an admitted runtime request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackedRequest {
    /// Nothing to send: the topic is already subscribed at this QoS.
    Unchanged,
    /// Recorded as pending; the next connect work or catch-up sends it.
    Stored,
    /// Recorded as pending and the session is live: send it now.
    SendNow,
    /// The runtime list is full.
    Full,
}

/// Which runtime subscriptions the current session still has to receive (ADR 023, rung 1).
///
/// The tracker holds no topics: a tier keeps them in its own bounded storage at the
/// same indices (registration order, entries never removed) and asks the tracker
/// what is owed.
/// `N` is the runtime capacity; an entry is `pending` until a SUBSCRIBE for its
/// current QoS went out on the current session.
///
/// Flow per connect: [`begin_connect`](Self::begin_connect), the plan's sends with
/// [`mark_sent`](Self::mark_sent), then [`catch_up_and_go_live`](Self::catch_up_and_go_live)
/// (or [`next_pending`](Self::next_pending) in a tier with no live flag).
/// A send that fails leaves its entry pending, so a later session, kept or not, still sends it.
#[derive(Debug, Clone)]
pub struct SubscriptionTracker<const N: usize> {
    qos: [QoS; N],
    pending: [bool; N],
    len: usize,
    live: bool,
}

impl<const N: usize> Default for SubscriptionTracker<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> SubscriptionTracker<N> {
    /// An empty tracker that is not live.
    pub const fn new() -> Self {
        Self {
            qos: [QoS::AtMostOnce; N],
            pending: [false; N],
            len: 0,
            live: false,
        }
    }

    /// Number of runtime entries.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// `true` when no runtime entry exists.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The QoS stored for entry `index`.
    pub fn qos(&self, index: usize) -> Option<QoS> {
        (index < self.len).then(|| self.qos[index])
    }

    /// Records an admission decision made by [`admit_subscription`].
    ///
    /// For [`Added`](SubscriptionAdmission::Added) the new entry gets index
    /// [`len`](Self::len) - 1 and the tier must store its topic there.
    pub fn apply(&mut self, admission: SubscriptionAdmission, qos: QoS) -> TrackedRequest {
        match admission {
            SubscriptionAdmission::Unchanged => return TrackedRequest::Unchanged,
            SubscriptionAdmission::Full => return TrackedRequest::Full,
            SubscriptionAdmission::Changed(index) => {
                if index >= self.len {
                    return TrackedRequest::Full;
                }
                self.qos[index] = qos;
                self.pending[index] = true;
            }
            SubscriptionAdmission::Added => {
                if self.len >= N {
                    return TrackedRequest::Full;
                }
                self.qos[self.len] = qos;
                self.pending[self.len] = true;
                self.len += 1;
            }
        }
        if self.live {
            TrackedRequest::SendNow
        } else {
            TrackedRequest::Stored
        }
    }

    /// A connect work starts: the session is not live yet, and one that needs
    /// every subscription again is owed every runtime entry.
    pub fn begin_connect(&mut self, resubscribe_all: bool) {
        self.live = false;
        if resubscribe_all {
            self.pending[..self.len].fill(true);
        }
    }

    /// The session ended or the link dropped: requests wait for the next connect.
    pub fn go_offline(&mut self) {
        self.live = false;
    }

    /// `true` between a finished catch-up and the next connect or drop.
    pub const fn is_live(&self) -> bool {
        self.live
    }

    /// Records that `qos` was sent for entry `index`.
    ///
    /// An entry whose QoS changed since stays pending, so a request that raced
    /// the send is not lost.
    pub fn mark_sent(&mut self, index: usize, qos: QoS) {
        if index < self.len && self.qos[index] == qos {
            self.pending[index] = false;
        }
    }

    /// First entry the session still has to receive, with the QoS to send.
    pub fn next_pending(&self) -> Option<(usize, QoS)> {
        (0..self.len)
            .find(|&i| self.pending[i])
            .map(|i| (i, self.qos[i]))
    }

    /// First pending entry at or after `start`, with the QoS to send.
    ///
    /// A tier that leaves several SUBSCRIBE packets unacknowledged walks the
    /// pending entries with it, so an entry already sent is not picked twice.
    pub fn next_pending_from(&self, start: usize) -> Option<(usize, QoS)> {
        (start..self.len)
            .find(|&i| self.pending[i])
            .map(|i| (i, self.qos[i]))
    }

    /// Sends entry `index`'s newest owed QoS through `send(qos)` and marks it sent.
    ///
    /// Does nothing and returns `true` when nothing is owed.
    /// A failed send leaves the entry owed and returns `false`.
    pub fn send_owed(&mut self, index: usize, send: impl FnOnce(QoS) -> bool) -> bool {
        if index >= self.len || !self.pending[index] {
            return true;
        }
        let qos = self.qos[index];
        if !send(qos) {
            return false;
        }
        self.mark_sent(index, qos);
        true
    }

    /// Sends every pending entry through `send(index, qos)`, then goes live.
    ///
    /// Returns `false` and stays not live at the first failed send; that entry
    /// stays pending.
    /// The caller holds the tracker's lock for the whole call, so a request is
    /// either sent here or sees `live` and is sent by its own caller.
    pub fn catch_up_and_go_live(&mut self, mut send: impl FnMut(usize, QoS) -> bool) -> bool {
        while let Some((index, qos)) = self.next_pending() {
            if !send(index, qos) {
                return false;
            }
            self.mark_sent(index, qos);
        }
        self.live = true;
        true
    }
}

/// Most SUBSCRIBE packets a tier may leave unacknowledged at once.
///
/// It is the in-flight capacity of the `minimq 0.13.5` session the bare-metal
/// tier drives (`MAX_RETAINED`, which a SUBSCRIBE occupies until its SUBACK
/// arrives); a ninth request would fail with an exhausted-capacity error.
/// Replayed QoS 1 publications of a kept session share that capacity, so a
/// tier also treats an exhausted-capacity refusal as "no room yet".
pub const SUBSCRIBE_WINDOW: usize = 8;

/// What a tier's client reports about one sent SUBSCRIBE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckStatus {
    /// The SUBACK has not arrived yet.
    Pending,
    /// The SUBACK arrived and granted the subscription.
    Complete,
    /// The session state that could complete it was discarded.
    Lost,
}

/// The SUBSCRIBE packets one connect work (or catch-up) has sent and not seen
/// acknowledged yet (ADR 023, rung 1).
///
/// A tier asks [`has_room`](Self::has_room) before it sends and, while there is
/// none, drives its client and calls [`settle`](Self::settle); it drains the
/// window the same way before the startup message and before it reports the
/// session ready.
/// `T` is whatever the tier needs to ask its client about the packet later (a
/// handle plus the QoS that went out); `N` is the window, normally
/// [`SUBSCRIBE_WINDOW`].
#[derive(Debug, Clone)]
pub struct SubscribeWindow<T: Copy, const N: usize> {
    slots: [Option<(usize, T)>; N],
}

impl<T: Copy, const N: usize> Default for SubscribeWindow<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Copy, const N: usize> SubscribeWindow<T, N> {
    /// An empty window.
    pub const fn new() -> Self {
        Self { slots: [None; N] }
    }

    /// Number of unacknowledged SUBSCRIBE packets.
    pub fn len(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    /// `true` when every sent SUBSCRIBE has been acknowledged.
    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(Option::is_none)
    }

    /// `true` when another SUBSCRIBE may be sent without exceeding the window.
    pub fn has_room(&self) -> bool {
        self.slots.iter().any(Option::is_none)
    }

    /// Records a sent SUBSCRIBE under the caller's `tag`.
    ///
    /// Returns `false` when the window is full; the caller must have checked
    /// [`has_room`](Self::has_room).
    pub fn track(&mut self, tag: usize, handle: T) -> bool {
        match self.slots.iter_mut().find(|slot| slot.is_none()) {
            Some(slot) => {
                *slot = Some((tag, handle));
                true
            }
            None => false,
        }
    }

    /// Releases every acknowledged entry, calling `complete(tag, handle)` for each.
    ///
    /// `status` asks the client about one handle.
    /// Returns `Err(tag)` for the first entry whose state was lost; it is
    /// released too, and the session must end.
    pub fn settle(
        &mut self,
        mut status: impl FnMut(&T) -> AckStatus,
        mut complete: impl FnMut(usize, T),
    ) -> Result<(), usize> {
        let mut lost = None;
        for slot in &mut self.slots {
            let Some((tag, handle)) = *slot else { continue };
            match status(&handle) {
                AckStatus::Pending => {}
                AckStatus::Complete => {
                    *slot = None;
                    complete(tag, handle);
                }
                AckStatus::Lost => {
                    *slot = None;
                    lost.get_or_insert(tag);
                }
            }
        }
        lost.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SubscriptionAdmission as A;

    const Q0: QoS = QoS::AtMostOnce;
    const Q1: QoS = QoS::AtLeastOnce;

    #[test]
    fn a_new_topic_is_added() {
        assert_eq!(admit_subscription([], [], "a", Q1, 2), A::Added);
        assert_eq!(
            admit_subscription([("b", Q1)], [("c", Q1)], "a", Q1, 2),
            A::Added
        );
    }

    #[test]
    fn a_runtime_duplicate_is_unchanged_or_changed_by_qos() {
        assert_eq!(
            admit_subscription([], [("x", Q1), ("a", Q1)], "a", Q1, 4),
            A::Unchanged
        );
        assert_eq!(
            admit_subscription([], [("x", Q1), ("a", Q0)], "a", Q1, 4),
            A::Changed(1)
        );
    }

    #[test]
    fn a_builder_duplicate_is_unchanged_only_at_the_same_qos() {
        assert_eq!(
            admit_subscription([("a", Q1)], [], "a", Q1, 4),
            A::Unchanged
        );
        assert_eq!(admit_subscription([("a", Q0)], [], "a", Q1, 4), A::Added);
    }

    #[test]
    fn a_full_runtime_list_refuses_new_topics_only() {
        let runtime = [("a", Q1), ("b", Q1)];
        assert_eq!(admit_subscription([], runtime, "c", Q1, 2), A::Full);
        assert_eq!(admit_subscription([], runtime, "b", Q1, 2), A::Unchanged);
        assert_eq!(admit_subscription([], runtime, "b", Q0, 2), A::Changed(1));
        assert_eq!(admit_subscription([], [], "a", Q1, 0), A::Full);
    }

    #[test]
    fn the_fixed_list_does_not_count_against_the_capacity() {
        let fixed = [("a", Q1), ("b", Q1), ("c", Q1)];
        assert_eq!(admit_subscription(fixed, [], "d", Q1, 1), A::Added);
    }

    type T = SubscriptionTracker<2>;

    fn add(t: &mut T, qos: QoS) -> TrackedRequest {
        t.apply(A::Added, qos)
    }

    #[test]
    fn an_added_entry_is_pending_until_sent_at_its_qos() {
        let mut t = T::new();
        assert_eq!(add(&mut t, Q1), TrackedRequest::Stored);
        assert_eq!(t.next_pending(), Some((0, Q1)));
        t.mark_sent(0, Q1);
        assert_eq!(t.next_pending(), None);
        assert_eq!(t.apply(A::Unchanged, Q1), TrackedRequest::Unchanged);
    }

    #[test]
    fn a_qos_change_between_read_and_mark_sent_stays_pending() {
        let mut t = T::new();
        add(&mut t, Q0);
        let (index, read_qos) = t.next_pending().unwrap();
        assert_eq!(t.apply(A::Changed(index), Q1), TrackedRequest::Stored);
        t.mark_sent(index, read_qos);
        assert_eq!(t.next_pending(), Some((0, Q1)));
        let mut sent = [None; 2];
        assert!(t.catch_up_and_go_live(|i, q| {
            sent[i] = Some(q);
            true
        }));
        assert_eq!(sent[0], Some(Q1));
        assert!(t.is_live());
    }

    #[test]
    fn a_live_tracker_asks_for_an_immediate_send() {
        let mut t = T::new();
        assert!(t.catch_up_and_go_live(|_, _| true));
        assert_eq!(add(&mut t, Q1), TrackedRequest::SendNow);
        t.go_offline();
        assert_eq!(t.apply(A::Changed(0), Q0), TrackedRequest::Stored);
    }

    #[test]
    fn a_kept_session_with_nothing_to_send_is_live_after_catch_up() {
        // The empty connect plan of a kept session: no builder subscriptions
        // to redo, no startup, no callback. The closing step still has to run,
        // or every later runtime request is stored instead of sent.
        let mut t = T::new();
        t.begin_connect(false);
        assert_eq!(t.next_pending(), None);
        assert!(!t.is_live());
        let mut sends = 0;
        assert!(t.catch_up_and_go_live(|_, _| {
            sends += 1;
            true
        }));
        assert_eq!(sends, 0, "nothing was owed");
        assert!(t.is_live());
        assert_eq!(add(&mut t, Q1), TrackedRequest::SendNow);
    }

    #[test]
    fn a_kept_session_sends_only_what_is_still_owed() {
        let mut t = T::new();
        add(&mut t, Q1);
        add(&mut t, Q1);
        t.mark_sent(0, Q1);
        t.begin_connect(false);
        let mut sent = heapless_sent();
        assert!(t.catch_up_and_go_live(|i, _| {
            sent.push(i);
            true
        }));
        assert_eq!(sent.as_slice(), &[1]);
    }

    #[test]
    fn a_fresh_session_is_owed_every_entry() {
        let mut t = T::new();
        add(&mut t, Q1);
        add(&mut t, Q0);
        t.mark_sent(0, Q1);
        t.mark_sent(1, Q0);
        t.begin_connect(true);
        assert_eq!(t.next_pending(), Some((0, Q1)));
        t.mark_sent(0, Q1);
        assert_eq!(t.next_pending(), Some((1, Q0)));
    }

    #[test]
    fn a_failed_send_stays_pending_through_a_kept_session_reconnect() {
        let mut t = T::new();
        add(&mut t, Q1);
        t.begin_connect(true);
        t.mark_sent(0, Q1);
        assert_eq!(t.apply(A::Changed(0), Q0), TrackedRequest::Stored);
        assert!(!t.catch_up_and_go_live(|_, _| false));
        assert!(!t.is_live());
        t.go_offline();
        t.begin_connect(false);
        assert_eq!(t.next_pending(), Some((0, Q0)));
        let mut calls = 0;
        assert!(t.catch_up_and_go_live(|_, _| {
            calls += 1;
            true
        }));
        assert_eq!(calls, 1);
    }

    #[test]
    fn the_tracker_is_bounded() {
        let mut t = T::new();
        add(&mut t, Q1);
        add(&mut t, Q1);
        assert_eq!(add(&mut t, Q1), TrackedRequest::Full);
        assert_eq!(t.apply(A::Full, Q1), TrackedRequest::Full);
        assert_eq!(t.len(), 2);
        assert_eq!(t.qos(1), Some(Q1));
        assert_eq!(t.qos(2), None);
    }

    #[test]
    fn next_pending_from_skips_entries_already_walked() {
        let mut t = T::new();
        add(&mut t, Q1);
        add(&mut t, Q0);
        assert_eq!(t.next_pending_from(0), Some((0, Q1)));
        assert_eq!(t.next_pending_from(1), Some((1, Q0)));
        assert_eq!(t.next_pending_from(2), None);
        t.mark_sent(1, Q0);
        assert_eq!(t.next_pending_from(1), None);
        assert_eq!(t.next_pending_from(99), None);
    }

    /// A broker connection as `minimq` shows it: SUBSCRIBE packets occupy
    /// `capacity` retained slots until their SUBACK, and `poll` makes one unit
    /// of progress (an inbound publish first, when configured, then one SUBACK).
    struct FakeConn {
        capacity: usize,
        next_id: u32,
        outstanding: alloc::vec::Vec<u32>,
        reject: Option<u32>,
        inbound_per_poll: usize,
        delivered: usize,
        max_outstanding: usize,
    }

    impl FakeConn {
        fn new(capacity: usize) -> Self {
            Self {
                capacity,
                next_id: 1,
                outstanding: alloc::vec::Vec::new(),
                reject: None,
                inbound_per_poll: 0,
                delivered: 0,
                max_outstanding: 0,
            }
        }

        fn subscribe(&mut self) -> Result<u32, &'static str> {
            if self.outstanding.len() >= self.capacity {
                return Err("inflight exhausted");
            }
            let id = self.next_id;
            self.next_id += 1;
            self.outstanding.push(id);
            self.max_outstanding = self.max_outstanding.max(self.outstanding.len());
            Ok(id)
        }

        fn poll(&mut self) -> Result<(), &'static str> {
            self.delivered += self.inbound_per_poll;
            if self.outstanding.is_empty() {
                return Ok(());
            }
            let id = self.outstanding.remove(0);
            if self.reject == Some(id) {
                return Err("subscribe refused");
            }
            Ok(())
        }

        fn status(&self, id: u32) -> AckStatus {
            if self.outstanding.contains(&id) {
                AckStatus::Pending
            } else {
                AckStatus::Complete
            }
        }
    }

    /// The connect-work loop of a tier, with the decisions taken from the window.
    fn subscribe_all(
        conn: &mut FakeConn,
        total: usize,
        marked: &mut alloc::vec::Vec<usize>,
    ) -> Result<(), &'static str> {
        let mut window = SubscribeWindow::<u32, SUBSCRIBE_WINDOW>::new();
        let mut step = |conn: &mut FakeConn,
                        window: &mut SubscribeWindow<u32, SUBSCRIBE_WINDOW>|
         -> Result<(), &'static str> {
            conn.poll()?;
            window
                .settle(|id| conn.status(*id), |tag, _| marked.push(tag))
                .map_err(|_| "lost")
        };
        for index in 0..total {
            while !window.has_room() {
                step(conn, &mut window)?;
            }
            let id = conn.subscribe()?;
            assert!(window.track(index, id));
        }
        while !window.is_empty() {
            step(conn, &mut window)?;
        }
        Ok(())
    }

    #[test]
    fn more_subscriptions_than_the_window_are_paced_and_all_acknowledged() {
        for total in [8, 9, 16, 17] {
            let mut conn = FakeConn::new(SUBSCRIBE_WINDOW);
            let mut marked = alloc::vec::Vec::new();
            assert_eq!(subscribe_all(&mut conn, total, &mut marked), Ok(()));
            assert!(conn.max_outstanding <= SUBSCRIBE_WINDOW);
            marked.sort_unstable();
            assert_eq!(marked, (0..total).collect::<alloc::vec::Vec<_>>());
        }
    }

    #[test]
    fn a_rejected_suback_ends_the_work_before_it_is_ready() {
        let mut conn = FakeConn::new(SUBSCRIBE_WINDOW);
        conn.reject = Some(3);
        let mut marked = alloc::vec::Vec::new();
        assert_eq!(
            subscribe_all(&mut conn, 9, &mut marked),
            Err("subscribe refused")
        );
        assert!(!marked.contains(&2));
        assert!(marked.len() < 9);
    }

    #[test]
    fn inbound_publishes_between_subacks_are_not_lost() {
        let mut conn = FakeConn::new(SUBSCRIBE_WINDOW);
        conn.inbound_per_poll = 1;
        let mut marked = alloc::vec::Vec::new();
        assert_eq!(subscribe_all(&mut conn, 12, &mut marked), Ok(()));
        assert_eq!(marked.len(), 12);
        assert_eq!(conn.delivered, 12);
    }

    #[test]
    fn a_window_reports_a_lost_entry_and_frees_it() {
        let mut w = SubscribeWindow::<u8, 2>::new();
        assert!(w.has_room() && w.is_empty());
        assert!(w.track(5, 1));
        assert!(w.track(6, 2));
        assert!(!w.has_room());
        assert!(!w.track(7, 3));
        assert_eq!(w.len(), 2);
        let mut done = alloc::vec::Vec::new();
        let result = w.settle(
            |h| {
                if *h == 1 {
                    AckStatus::Lost
                } else {
                    AckStatus::Complete
                }
            },
            |tag, _| done.push(tag),
        );
        assert_eq!(result, Err(5));
        assert_eq!(done, [6]);
        assert!(w.is_empty());
    }

    /// Two callers change one topic's QoS (A, then B); the tier transmits in
    /// the order its client lock lets them through.
    fn admit_two(t: &mut T, first: QoS, second: QoS) {
        t.begin_connect(true);
        assert!(t.catch_up_and_go_live(|_, _| true));
        assert_eq!(t.apply(A::Added, Q0), TrackedRequest::SendNow);
        assert!(t.catch_up_and_go_live(|_, _| true));
        assert_eq!(t.apply(A::Changed(0), first), TrackedRequest::SendNow);
        assert_eq!(t.apply(A::Changed(0), second), TrackedRequest::SendNow);
    }

    #[test]
    fn a_stale_transmit_after_a_newer_one_cannot_leave_the_broker_on_the_old_qos() {
        let mut t = T::new();
        admit_two(&mut t, Q0, Q1);
        let mut broker = None;
        let mut transmit = |t: &mut T| {
            t.send_owed(0, |qos| {
                broker = Some(qos);
                true
            });
        };
        transmit(&mut t);
        transmit(&mut t);
        assert_eq!(broker, Some(Q1));
        assert_eq!(t.next_pending(), None);
    }

    #[test]
    fn the_newest_qos_wins_in_either_transmit_order() {
        for (first, second) in [(Q0, Q1), (Q1, Q0)] {
            let mut t = T::new();
            admit_two(&mut t, first, second);
            let mut sent = alloc::vec::Vec::new();
            for _ in 0..2 {
                t.send_owed(0, |qos| {
                    sent.push(qos);
                    true
                });
            }
            assert_eq!(sent, [second]);
            assert_eq!(t.qos(0), Some(second));
            assert_eq!(t.next_pending(), None);
        }
    }

    #[test]
    fn a_failed_send_keeps_the_entry_owed() {
        let mut t = T::new();
        admit_two(&mut t, Q0, Q1);
        assert!(!t.send_owed(0, |_| false));
        assert_eq!(t.next_pending(), Some((0, Q1)));
        assert!(t.send_owed(0, |_| true));
        assert_eq!(t.next_pending(), None);
    }

    fn heapless_sent() -> SentIndices {
        SentIndices([0; 4], 0)
    }

    struct SentIndices([usize; 4], usize);

    impl SentIndices {
        fn push(&mut self, i: usize) {
            self.0[self.1] = i;
            self.1 += 1;
        }

        fn as_slice(&self) -> &[usize] {
            &self.0[..self.1]
        }
    }
}
