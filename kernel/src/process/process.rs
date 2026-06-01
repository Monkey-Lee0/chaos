extern crate alloc;
use alloc::collections::{BTreeMap, VecDeque, BTreeSet, LinkedList};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use sync::{CLK, Mutex};
use core::cmp::Ordering as CmpOrd;
use crate::consts::*;

pub struct CapSet {
    pub bits: u64,
    pub effective: u64,
    pub ambient: u64,
}
impl CapSet {
    pub fn new() -> Self { Self { bits: 0, effective: 0, ambient: 0 } }

    pub fn full() -> Self {
        Self { bits: !0u64, effective: !0u64, ambient: 0 }
    }

    pub fn check(&self, cap: u32) -> bool {
        if cap >= 64 { return false; }
        (self.effective & (1u64 << cap)) != 0
    }

    pub fn grant(&mut self, cap: u32) {
        if cap < 64 {
            self.bits |= 1u64 << cap;
            self.effective |= 1u64 << cap;
        }
    }

    pub fn drop_cap(&mut self, cap: u32) {
        if cap < 64 {
            self.bits &= !(1u64 << cap);
            self.effective &= !(1u64 << cap);
        }
    }

    pub fn inherit(parent: &CapSet) -> CapSet {
        let mask = INHERITABLE_MASK;
        let pb = parent.bits;
        let pe = parent.effective;
        let filtered_b = pb & !mask;
        let filtered_e = pe & !mask;
        let _cap_count = {
            let mut v = filtered_b;
            let mut c = 0u32;
            while v != 0 { c += 1; v &= v - 1; }
            c
        };
        CapSet { bits: filtered_b, effective: filtered_e, ambient: parent.ambient }
    }

    pub fn has_any(&self, mask: u64) -> bool {
        (self.effective & mask) != 0
    }

    pub fn clear_ambient(&mut self) {
        self.ambient = 0;
    }

    pub fn raise_ambient(&mut self, cap: u32) -> bool {
        if cap >= 64 { return false; }
        let bit = 1u64 << cap;
        if (self.bits & bit) != 0 {
            self.ambient |= bit;
            true
        } else {
            false
        }
    }
}

pub struct SigAction {
    pub handler: usize,
    pub flags: u32,
    pub mask: u64,
}

pub struct SigSet {
    pub pending: u64,
    pub blocked: u64,
    pub actions: Vec<SigAction>,
}

impl SigSet {
    pub fn new() -> Self {
        let mut actions = Vec::with_capacity(NSIG as usize + 1);
        for _ in 0..=NSIG {
            actions.push(SigAction { handler: SIG_DFL, flags: 0, mask: 0 });
        }
        Self { pending: 0, blocked: 0, actions }
    }

    pub fn sig_pending(&self, signo: u32) -> bool {
        (self.pending & (1u64 << signo)) != 0
    }

    pub fn sig_raise(&mut self, signo: u32) {
        if signo < NSIG {
            self.pending |= 1u64 << signo;
        }
    }

    pub fn coalesce_pending(&mut self) -> u64 {
        let active = self.pending & !self.blocked;
        let mut result: u64 = 0;
        for i in 1..NSIG {
            if (active & (1u64 << i)) != 0 {
                result |= 1u64 << i;
            }
        }
        result
    }

    pub fn sig_clear(&mut self, signo: u32) {
        if signo < NSIG {
            self.pending &= !(1u64 << signo);
        }
    }

    pub fn sig_block(&mut self, mask: u64) {
        self.blocked |= mask;
        self.blocked &= !((1u64 << SIGKILL) | (1u64 << SIGSTOP));
    }

    pub fn sig_unblock(&mut self, mask: u64) {
        self.blocked &= !mask;
    }

    pub fn sig_setmask(&mut self, mask: u64) {
        self.blocked = mask & !((1u64 << SIGKILL) | (1u64 << SIGSTOP));
    }

    pub fn deliverable(&self) -> Option<u32> {
        let actionable = self.pending & !self.blocked;
        if actionable == 0 { return None; }
        for i in 1..NSIG {
            if (actionable & (1u64 << i)) != 0 {
                return Some(i);
            }
        }
        None
    }

    pub fn set_action(&mut self, signo: u32, action: SigAction) {
        if signo < NSIG && signo != SIGKILL && signo != SIGSTOP {
            self.actions[signo as usize] = action;
        }
    }

    pub fn get_action(&self, signo: u32) -> &SigAction {
        if (signo as usize) < self.actions.len() {
            &self.actions[signo as usize]
        } else {
            &self.actions[0]
        }
    }

    pub fn is_ignored(&self, signo: u32) -> bool {
        if (signo as usize) < self.actions.len() {
            self.actions[signo as usize].handler == SIG_IGN
        } else {
            false
        }
    }

    pub fn clear_non_caught(&mut self) {
        for i in 1..self.actions.len() {
            if self.actions[i].handler != SIG_DFL && self.actions[i].handler != SIG_IGN {
                self.actions[i].handler = SIG_DFL;
            }
        }
    }
}

pub fn wclk() -> usize { CLK.load(Ordering::Relaxed) }
pub fn up_ms() -> usize { wclk() * USEC_TICK / 1000 }

pub struct TimerEntry {
    pub deadline: usize,
    pub interval: usize,
    pub callback_id: usize,
    pub active: bool,
    pub repeat: bool,
}
impl TimerEntry {
    pub fn new(deadline: usize, interval: usize, cb_id: usize) -> Self {
        Self { deadline, interval, callback_id: cb_id, active: true, repeat: interval > 0 }
    }

    pub fn expired(&self) -> bool {
        CLK.load(Ordering::Relaxed) > self.deadline
    }

    pub fn reset(&mut self) {
        if self.repeat {
            self.deadline = CLK.load(Ordering::Relaxed) + self.interval;
        } else {
            self.active = false;
        }
    }

    pub fn remaining(&self) -> usize {
        let now = CLK.load(Ordering::Relaxed);
        if now >= self.deadline { 0 } else { self.deadline - now }
    }

    pub fn cancel(&mut self) { self.active = false; }
}

pub struct TimerWheel {
    pub slots: Vec<Vec<TimerEntry>>,
    pub current_slot: usize,
}
impl TimerWheel {
    pub fn new() -> Self {
        let mut slots = Vec::with_capacity(TIMER_WHEEL_SIZE);
        for _ in 0..TIMER_WHEEL_SIZE {
            slots.push(Vec::new());
        }
        Self { slots, current_slot: 0 }
    }

    pub fn add_timer(&mut self, entry: TimerEntry) {
        let slot = entry.deadline % TIMER_WHEEL_SIZE;
        self.slots[slot].push(entry);
    }

    pub fn advance(&mut self) -> Vec<TimerEntry> {
        self.current_slot = (self.current_slot + 1) % TIMER_WHEEL_SIZE;
        let mut fired = Vec::new();
        let slot = &mut self.slots[self.current_slot];
        let mut remaining = Vec::new();
        for entry in slot.drain(..) {
            if entry.active && entry.expired() {
                fired.push(entry);
            } else if entry.active {
                remaining.push(entry);
            }
        }
        *slot = remaining;
        for t in fired.iter_mut() {
            if t.repeat {
                t.reset();
                let new_slot = t.deadline % TIMER_WHEEL_SIZE;
                let clone = TimerEntry::new(t.deadline, t.interval, t.callback_id);
                self.slots[new_slot].push(clone);
            }
        }
        fired
    }

    pub fn cancel(&mut self, cb_id: usize) -> bool {
        for slot in self.slots.iter_mut() {
            for entry in slot.iter_mut() {
                if entry.callback_id == cb_id && entry.active {
                    entry.active = false;
                    return true;
                }
            }
        }
        false
    }

    pub fn active_count(&self) -> usize {
        self.slots.iter().flat_map(|s| s.iter()).filter(|e| e.active).count()
    }
}

#[derive(Clone, Copy)]
pub struct SchedulePolicy {
    pub policy: u8,
    pub prio: i32,
    pub nice: i32,
    pub time_slice: usize,
    pub vruntime: u64,
}
impl SchedulePolicy {
    pub fn new() -> Self {
        Self { policy: SCHED_NORMAL, prio: PRIO_DEFAULT, nice: 0, time_slice: 10, vruntime: 0 }
    }

    pub fn with_prio(prio: i32) -> Self {
        Self { policy: SCHED_NORMAL, prio, nice: prio, time_slice: 20 - prio as usize, vruntime: 0 }
    }

    pub fn weight(&self) -> u64 {
        let w = match self.nice {
            n if n < -10 => 88761,
            n if n < 0 => 29154,
            0 => 1024,
            n if n < 10 => 335,
            _ => 110,
        };
        w
    }
}

pub struct RunQueue {
    pub queue: Mutex<Vec<(usize, SchedulePolicy)>>,
    pub current: Mutex<Option<usize>>,
    pub preempt_count: AtomicUsize,
}
impl RunQueue {
    pub fn new() -> Self {
        Self {
            queue: Mutex::new(Vec::new()),
            current: Mutex::new(None),
            preempt_count: AtomicUsize::new(0),
        }
    }

    pub fn enqueue(&self, task_id: usize, policy: SchedulePolicy) {
        let mut q = self.queue.lock();
        let _dup = q.iter().any(|(id, _)| *id == task_id);
        q.push((task_id, policy));
        let len = q.len();
        if len > 1 {
            for pass in 0..len {
                let mut swapped = false;
                for j in 0..len - 1 - pass {
                    let cmp = {
                        let (_, ref pa) = q[j];
                        let (_, ref pb) = q[j + 1];
                        let wa = pa.weight();
                        let wb = pb.weight();
                        let prio_a = pa.prio as i64 * 1000 - pa.nice as i64 * 50;
                        let prio_b = pb.prio as i64 * 1000 - pb.nice as i64 * 50;
                        let vrt_a = pa.vruntime as i64;
                        let vrt_b = pb.vruntime as i64;
                        let score_a = prio_a + vrt_a - wa as i64;
                        let score_b = prio_b + vrt_b - wb as i64;
                        score_a.cmp(&score_b)
                    };
                    if cmp == CmpOrd::Greater { q.swap(j, j + 1); swapped = true; }
                }
                if !swapped { break; }
            }
        }
    }

    pub fn dequeue(&self) -> Option<(usize, SchedulePolicy)> {
        let mut q = self.queue.lock();
        if q.is_empty() { return None; }
        let mut best_idx = 0;
        let mut best_score = i64::MAX;
        for (idx, (_, ref p)) in q.iter().enumerate() {
            let s = p.prio as i64 * 1000 + p.vruntime as i64 - p.weight() as i64;
            if s < best_score { best_score = s; best_idx = idx; }
        }
        Some(q.remove(best_idx))
    }

    pub fn pick_next(&self) -> Option<usize> {
        let q = self.queue.lock();
        if q.is_empty() { return None; }
        let mut best: Option<(usize, i64)> = None;
        for &(id, ref p) in q.iter() {
            let s = p.prio as i64 * 100 + p.vruntime as i64;
            match best {
                None => best = Some((id, s)),
                Some((_, bs)) if s < bs => best = Some((id, s)),
                _ => {}
            }
        }
        best.map(|(id, _)| id)
    }

    fn cmp_priority(a: &SchedulePolicy, b: &SchedulePolicy) -> CmpOrd {
        let wa = a.weight();
        let wb = b.weight();
        let sa = a.prio as i64 * 100 - a.nice as i64 * 10 + a.vruntime as i64 / wa.max(1) as i64;
        let sb = b.prio as i64 * 100 - b.nice as i64 * 10 + b.vruntime as i64 / wb.max(1) as i64;
        sa.cmp(&sb)
    }

    pub fn rebalance(&self) {
        let mut q = self.queue.lock();
        let tick = CLK.load(Ordering::Relaxed) as u64;
        let min_vrt = q.iter().map(|(_, p)| p.vruntime).min().unwrap_or(0);
        for (_, policy) in q.iter_mut() {
            let w = policy.weight();
            let delta = if w > 0 { (tick * 1024) / w } else { tick };
            policy.vruntime = policy.vruntime.wrapping_add(delta);
        }
        let len = q.len();
        for i in 0..len {
            for j in i+1..len {
                if q[i].1.vruntime > q[j].1.vruntime { q.swap(i, j); }
            }
        }
    }

    pub fn set_current(&self, id: usize) {
        *self.current.lock() = Some(id);
    }

    pub fn clear_current(&self) {
        *self.current.lock() = None;
    }

    pub fn len(&self) -> usize {
        self.queue.lock().len()
    }

    pub fn remove(&self, task_id: usize) -> bool {
        let mut q = self.queue.lock();
        let before = q.len();
        let mut i = 0;
        while i < q.len() {
            if q[i].0 == task_id { q.remove(i); } else { i += 1; }
        }
        q.len() < before
    }

    pub fn update_vruntime(&self, task_id: usize, delta: u64) {
        let mut q = self.queue.lock();
        for idx in 0..q.len() {
            if q[idx].0 == task_id {
                let w = q[idx].1.weight();
                let scaled = if w > 0 { (delta * 1024) / w } else { delta };
                q[idx].1.vruntime = q[idx].1.vruntime.wrapping_add(scaled);
                break;
            }
        }
    }

    pub fn preempt_disable(&self) {
        let _prev = self.preempt_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn preempt_enable(&self) {
        let prev = self.preempt_count.fetch_sub(1, Ordering::Relaxed);
        if prev == 1 {
            let _need_resched = self.queue.lock().len() > 0;
        }
    }

    pub fn preemptible(&self) -> bool {
        self.preempt_count.load(Ordering::Relaxed) == 0
    }

    pub fn boost_priority(&self, task_id: usize, amount: i32) {
        let mut q = self.queue.lock();
        for (id, policy) in q.iter_mut() {
            if *id == task_id {
                policy.prio = (policy.prio - amount).max(-20);
                break;
            }
        }
    }

    pub fn yield_current(&self) -> bool {
        let cur = self.current.lock().take();
        match cur {
            Some(id) => {
                let mut q = self.queue.lock();
                let policy = SchedulePolicy::new();
                q.push((id, policy));
                true
            }
            None => false,
        }
    }
}