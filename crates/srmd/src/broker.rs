//! Who gets a slot, and when.
//!
//! No I/O and no clock of its own: every entry point is handed the time and returns
//! what changed, so the scheduling rules can be tested as plain functions.
//!
//! ## The rule
//!
//! Requests queue first-come, first-served, with one refinement. Strict FIFO would
//! leave slots idle whenever the head of the queue is a big request that doesn't fit
//! yet, while small ones wait behind it. Free-for-all (what polling gives you) lets a
//! stream of small requests starve a big one forever. So:
//!
//! - While the head has waited less than the **grace period**, smaller requests may
//!   pass it into free slots.
//! - Once it has waited longer, it **reserves**: nothing behind it starts, the
//!   running work drains, and it gets in.
//!
//! So a request's wait is bounded by the longest job running when its grace ends.

use srm_proto::{Lease, LeaseId, LeaseRequest, Millis, Pool};
use std::collections::{BTreeMap, HashMap, VecDeque};

#[derive(Clone, Debug, PartialEq)]
pub struct Grant {
    pub lease: LeaseId,
    pub amount: u32,
    pub capacity: u32,
    pub waited_ms: u64,
    pub granted_at: Millis,
}

#[derive(Debug)]
pub struct Queued {
    pub lease: LeaseId,
    pub amount: u32,
    pub position: u32,
    pub free: u32,
    pub capacity: u32,
    pub holders: Vec<Lease>,
}

#[derive(Debug)]
pub enum Outcome {
    Queued(Queued),
    Granted(Grant),
}

struct PoolState {
    capacity: u32,
    holders: Vec<Lease>,
    queue: VecDeque<Lease>,
}

impl PoolState {
    fn used(&self) -> u32 {
        self.holders.iter().map(|l| l.amount).sum()
    }

    fn free(&self) -> u32 {
        self.capacity.saturating_sub(self.used())
    }
}

pub struct Broker {
    pools: BTreeMap<String, PoolState>,
    /// Which pool each live lease is in.
    index: HashMap<LeaseId, String>,
    next_id: LeaseId,
    grace_ms: Millis,
}

impl Broker {
    pub fn new(pools: BTreeMap<String, u32>, grace_ms: Millis) -> Self {
        Self {
            pools: pools
                .into_iter()
                .map(|(name, capacity)| {
                    (
                        name,
                        PoolState {
                            capacity,
                            holders: Vec::new(),
                            queue: VecDeque::new(),
                        },
                    )
                })
                .collect(),
            index: HashMap::new(),
            next_id: 1,
            grace_ms,
        }
    }

    /// Ask for a lease. A request larger than the pool is clamped to it rather than
    /// waiting for slots that can never exist.
    pub fn request(
        &mut self,
        request: LeaseRequest,
        repo: Option<String>,
        now: Millis,
    ) -> Result<Outcome, String> {
        let grace = self.grace_ms;
        let pool = self
            .pools
            .get_mut(&request.pool)
            .ok_or_else(|| format!("no pool named {:?}", request.pool))?;

        let id = self.next_id;
        self.next_id += 1;
        let amount = request.amount.clamp(1, pool.capacity);
        let capacity = pool.capacity;
        self.index.insert(id, request.pool.clone());
        pool.queue.push_back(Lease {
            id,
            pool: request.pool,
            amount,
            label: request.label,
            pid: request.pid,
            cwd: request.cwd,
            repo,
            session: request.session,
            low: request.low,
            requested_at: now,
            granted_at: None,
            usage: None,
        });

        // Only this request is new, so only it can be the one let in.
        let grants = schedule(pool, grace, now);
        debug_assert!(grants.iter().all(|g| g.lease == id));
        if let Some(grant) = grants.into_iter().next() {
            return Ok(Outcome::Granted(grant));
        }
        let position = pool.queue.len() as u32;
        Ok(Outcome::Queued(Queued {
            lease: id,
            amount,
            position,
            free: pool.free(),
            capacity,
            holders: pool.holders.clone(),
        }))
    }

    /// Bring back a lease that was already running under a previous daemon. It goes
    /// straight in, over capacity if need be: the work exists either way, and an
    /// accurate count matters more than a tidy one.
    pub fn adopt(
        &mut self,
        request: LeaseRequest,
        repo: Option<String>,
        requested_at: Millis,
        granted_at: Millis,
    ) -> Result<Grant, String> {
        let pool = self
            .pools
            .get_mut(&request.pool)
            .ok_or_else(|| format!("no pool named {:?}", request.pool))?;
        let id = self.next_id;
        self.next_id += 1;
        self.index.insert(id, request.pool.clone());
        let amount = request.amount.max(1);
        pool.holders.push(Lease {
            id,
            pool: request.pool,
            amount,
            label: request.label,
            pid: request.pid,
            cwd: request.cwd,
            repo,
            session: request.session,
            low: request.low,
            requested_at,
            granted_at: Some(granted_at),
            usage: None,
        });
        Ok(Grant {
            lease: id,
            amount,
            capacity: pool.capacity,
            waited_ms: 0,
            granted_at,
        })
    }

    /// End a lease, queued or held, and return it with whoever that let in.
    pub fn release(&mut self, id: LeaseId, now: Millis) -> (Option<Lease>, Vec<Grant>) {
        let Some(name) = self.index.remove(&id) else {
            return (None, Vec::new());
        };
        let grace = self.grace_ms;
        let Some(pool) = self.pools.get_mut(&name) else {
            return (None, Vec::new());
        };
        let lease = if let Some(i) = pool.holders.iter().position(|l| l.id == id) {
            Some(pool.holders.remove(i))
        } else if let Some(i) = pool.queue.iter().position(|l| l.id == id) {
            pool.queue.remove(i)
        } else {
            None
        };
        (lease, schedule(pool, grace, now))
    }

    /// Apply a new set of pools. Pools still in use survive a removal from the config
    /// until they drain, because their holders are real.
    pub fn reconfigure(
        &mut self,
        pools: BTreeMap<String, u32>,
        grace_ms: Millis,
        now: Millis,
    ) -> Vec<Grant> {
        self.grace_ms = grace_ms;
        for (name, capacity) in &pools {
            self.pools
                .entry(name.clone())
                .and_modify(|p| p.capacity = *capacity)
                .or_insert_with(|| PoolState {
                    capacity: *capacity,
                    holders: Vec::new(),
                    queue: VecDeque::new(),
                });
        }
        self.pools.retain(|name, p| {
            pools.contains_key(name) || !p.holders.is_empty() || !p.queue.is_empty()
        });
        let mut grants = Vec::new();
        for pool in self.pools.values_mut() {
            for waiter in &mut pool.queue {
                waiter.amount = waiter.amount.min(pool.capacity);
            }
            grants.extend(schedule(pool, grace_ms, now));
        }
        grants
    }

    pub fn lease_mut(&mut self, id: LeaseId) -> Option<&mut Lease> {
        let pool = self.pools.get_mut(self.index.get(&id)?)?;
        pool.holders.iter_mut().find(|l| l.id == id)
    }

    pub fn holders(&self) -> impl Iterator<Item = &Lease> {
        self.pools.values().flat_map(|p| p.holders.iter())
    }

    pub fn is_idle(&self) -> bool {
        self.index.is_empty()
    }

    pub fn snapshot(&self) -> Vec<Pool> {
        self.pools
            .iter()
            .map(|(name, p)| Pool {
                name: name.clone(),
                capacity: p.capacity,
                used: p.used(),
                holders: p.holders.clone(),
                waiters: p.queue.iter().cloned().collect(),
            })
            .collect()
    }
}

fn schedule(pool: &mut PoolState, grace_ms: Millis, now: Millis) -> Vec<Grant> {
    let mut grants = Vec::new();
    let mut free = pool.free();
    let mut i = 0;
    while i < pool.queue.len() {
        let waiter = &pool.queue[i];
        if waiter.amount <= free {
            let mut lease = pool.queue.remove(i).expect("index is in bounds");
            free -= lease.amount;
            lease.granted_at = Some(now);
            grants.push(Grant {
                lease: lease.id,
                amount: lease.amount,
                capacity: pool.capacity,
                waited_ms: (now - lease.requested_at).max(0) as u64,
                granted_at: now,
            });
            pool.holders.push(lease);
            continue;
        }
        if now - waiter.requested_at >= grace_ms {
            break; // It has waited its turn: reserve.
        }
        i += 1; // Still inside its grace, so the next may pass it.
    }
    grants
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRACE: Millis = 30_000;

    fn broker(capacity: u32) -> Broker {
        Broker::new(BTreeMap::from([("cpu".to_string(), capacity)]), GRACE)
    }

    fn ask(b: &mut Broker, amount: u32, at: Millis) -> Outcome {
        b.request(
            LeaseRequest {
                pool: "cpu".into(),
                amount,
                label: format!("{amount} at {at}"),
                pid: 1,
                cwd: None,
                session: None,
                low: false,
            },
            None,
            at,
        )
        .unwrap()
    }

    fn id(outcome: &Outcome) -> LeaseId {
        match outcome {
            Outcome::Queued(q) => q.lease,
            Outcome::Granted(g) => g.lease,
        }
    }

    fn granted(outcome: &Outcome) -> bool {
        matches!(outcome, Outcome::Granted(_))
    }

    #[test]
    fn grants_immediately_while_there_is_room() {
        let mut b = broker(4);
        assert!(granted(&ask(&mut b, 2, 0)));
        assert!(granted(&ask(&mut b, 2, 0)));
        assert!(!granted(&ask(&mut b, 1, 0)));
        assert_eq!(b.snapshot()[0].used, 4);
    }

    #[test]
    fn oversized_requests_are_clamped_not_deadlocked() {
        let mut b = broker(4);
        match ask(&mut b, 99, 0) {
            Outcome::Granted(g) => assert_eq!(g.amount, 4),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_zero_request_still_costs_one() {
        let mut b = broker(4);
        ask(&mut b, 0, 0);
        assert_eq!(b.snapshot()[0].used, 1);
    }

    #[test]
    fn release_lets_the_next_in_with_its_wait_recorded() {
        let mut b = broker(2);
        let holder = id(&ask(&mut b, 2, 0));
        let waiter = id(&ask(&mut b, 2, 1_000));
        let (lease, grants) = b.release(holder, 5_000);
        assert_eq!(lease.unwrap().id, holder);
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].lease, waiter);
        assert_eq!(grants[0].waited_ms, 4_000);
    }

    #[test]
    fn small_requests_backfill_past_a_big_one_during_grace() {
        let mut b = broker(4);
        let running = id(&ask(&mut b, 3, 0));
        let big = id(&ask(&mut b, 4, 1_000));
        // One slot is free and the big request can't use it yet: don't waste it.
        assert!(granted(&ask(&mut b, 1, 2_000)));
        let (_, grants) = b.release(running, 3_000);
        assert!(
            grants.iter().all(|g| g.lease != big),
            "1 slot is still held"
        );
    }

    #[test]
    fn once_grace_ends_the_head_reserves_and_nothing_passes() {
        let mut b = broker(4);
        let running = id(&ask(&mut b, 3, 0));
        let big = id(&ask(&mut b, 4, 0));
        let late = ask(&mut b, 1, GRACE + 1);
        assert!(!granted(&late), "the head has reserved the free slot");
        let (_, grants) = b.release(running, GRACE + 2);
        assert_eq!(grants[0].lease, big);
        assert_eq!(grants.len(), 1);
    }

    #[test]
    fn a_stream_of_small_requests_cannot_starve_a_big_one() {
        let mut b = broker(4);
        let mut held = vec![id(&ask(&mut b, 1, 0)), id(&ask(&mut b, 1, 0))];
        let big = id(&ask(&mut b, 4, 0));

        let mut now = 0;
        loop {
            now += 5_000;
            assert!(now < 120_000, "the big request starved");
            if b.snapshot()[0].holders.iter().any(|l| l.id == big) {
                break;
            }
            let oldest = held.remove(0);
            let (_, grants) = b.release(oldest, now);
            if grants.iter().any(|g| g.lease == big) {
                break;
            }
            let next = ask(&mut b, 1, now);
            if granted(&next) {
                held.push(id(&next));
            }
        }
    }

    #[test]
    fn leaving_the_queue_doesnt_disturb_the_rest() {
        let mut b = broker(1);
        let holder = id(&ask(&mut b, 1, 0));
        let gave_up = id(&ask(&mut b, 1, 0));
        let patient = id(&ask(&mut b, 1, 0));
        assert!(b.release(gave_up, 1).1.is_empty());
        let (_, grants) = b.release(holder, 2);
        assert_eq!(grants[0].lease, patient);
    }

    #[test]
    fn releasing_twice_is_harmless() {
        let mut b = broker(1);
        let holder = id(&ask(&mut b, 1, 0));
        assert!(b.release(holder, 1).0.is_some());
        assert!(b.release(holder, 2).0.is_none());
    }

    #[test]
    fn an_adopted_lease_counts_even_over_capacity() {
        let mut b = broker(2);
        ask(&mut b, 2, 0);
        let request = LeaseRequest {
            pool: "cpu".into(),
            amount: 3,
            label: "survivor".into(),
            pid: 1,
            cwd: None,
            session: None,
            low: false,
        };
        b.adopt(request, None, 0, 0).unwrap();
        let pool = &b.snapshot()[0];
        assert_eq!(pool.used, 5);
        assert!(
            !granted(&ask(&mut b, 1, 1)),
            "over budget: nothing else starts"
        );
    }

    #[test]
    fn pools_are_independent() {
        let mut b = Broker::new(
            BTreeMap::from([("cpu".to_string(), 1), ("camera".to_string(), 1)]),
            GRACE,
        );
        ask(&mut b, 1, 0);
        let camera = b.request(
            LeaseRequest {
                pool: "camera".into(),
                amount: 1,
                label: "stream".into(),
                pid: 1,
                cwd: None,
                session: None,
                low: false,
            },
            None,
            0,
        );
        assert!(granted(&camera.unwrap()));
    }

    #[test]
    fn an_unknown_pool_is_an_error_not_a_hang() {
        let mut b = broker(1);
        let request = LeaseRequest {
            pool: "nope".into(),
            amount: 1,
            label: "x".into(),
            pid: 1,
            cwd: None,
            session: None,
            low: false,
        };
        assert!(b.request(request, None, 0).is_err());
    }

    #[test]
    fn raising_capacity_lets_waiters_in() {
        let mut b = broker(1);
        ask(&mut b, 1, 0);
        let waiter = id(&ask(&mut b, 1, 0));
        let grants = b.reconfigure(BTreeMap::from([("cpu".to_string(), 2)]), GRACE, 1);
        assert_eq!(grants[0].lease, waiter);
    }

    #[test]
    fn a_pool_in_use_survives_removal_from_the_config_until_it_drains() {
        let mut b = Broker::new(
            BTreeMap::from([("cpu".to_string(), 1), ("camera".to_string(), 1)]),
            GRACE,
        );
        let request = LeaseRequest {
            pool: "camera".into(),
            amount: 1,
            label: "stream".into(),
            pid: 1,
            cwd: None,
            session: None,
            low: false,
        };
        let lease = id(&b.request(request, None, 0).unwrap());
        let only_cpu = BTreeMap::from([("cpu".to_string(), 1)]);
        b.reconfigure(only_cpu.clone(), GRACE, 1);
        assert_eq!(b.snapshot().len(), 2);
        b.release(lease, 2);
        b.reconfigure(only_cpu, GRACE, 3);
        assert_eq!(b.snapshot().len(), 1);
    }
}
