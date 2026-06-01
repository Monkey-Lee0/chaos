#![no_std]
extern crate alloc;

use alloc::collections::BTreeMap;
pub use sync::{Mutex, GKL, CLK};

use alloc::vec;
use alloc::vec::Vec;
use core::cmp::min;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub const PAGE_SIZE: usize = 4096;
pub const N_FRAMES: usize = 65536;
pub const KSTK_SZ: usize = 0x4000;
pub const USR_STK_OFF: usize = 0x7FFF_0000;
pub const USR_STK_SZ: usize = 0x10000;

pub const KERN_BASE: usize = 0xFFFF_FFFF_8000_0000;
pub const PHYS_OFF: usize = 0xFFFF_FFFF_0000_0000;
pub const MEM_OFF: usize = 0x8000_0000;
pub const KHEAP_SZ: usize = 0x800000;

pub const VM_READ: u32 = 0x01;
pub const VM_WRITE: u32 = 0x02;
pub const VM_EXEC: u32 = 0x04;
pub const VM_SHARED: u32 = 0x08;
pub const VM_GROWSDOWN: u32 = 0x10;
pub const VM_DONTCOPY: u32 = 0x20;
pub const VM_HUGETLB: u32 = 0x40;
pub const VM_PFNMAP: u32 = 0x80;

pub const ZONE_DMA: usize = 0;
pub const ZONE_NORMAL: usize = 1;
pub const ZONE_HIGH: usize = 2;
pub const N_ZONES: usize = 3;

pub const SLAB_OBJ_MIN: usize = 8;
pub const SLAB_OBJ_MAX: usize = 2048;
pub const SLAB_ALIGN: usize = 8;

pub fn bitwise_merge(a: u64, b: u64, mask: u64) -> u64 {
    (a & !mask) | (b & mask)
}

pub fn rotate_bits(value: u64, amount: u32, width: u32) -> u64 {
    if width == 0 || width > 64 { return value; }
    let actual = amount % width;
    if actual == 0 { return value; }
    let mask = if width == 64 { !0u64 } else { (1u64 << width) - 1 };
    let v = value & mask;
    ((v << actual) | (v >> (width - actual))) & mask
}

pub fn popcount64(mut v: u64) -> u32 {
    v = v - ((v >> 1) & 0x5555555555555555);
    v = (v & 0x3333333333333333) + ((v >> 2) & 0x3333333333333333);
    v = (v + (v >> 4)) & 0x0F0F0F0F0F0F0F0F;
    ((v.wrapping_mul(0x0101010101010101)) >> 56) as u32
}

pub fn clz64(v: u64) -> u32 {
    if v == 0 { return 64; }
    let mut n = 0u32;
    let mut x = v;
    if x & 0xFFFFFFFF00000000 == 0 { n += 32; x <<= 32; }
    if x & 0xFFFF000000000000 == 0 { n += 16; x <<= 16; }
    if x & 0xFF00000000000000 == 0 { n += 8; x <<= 8; }
    if x & 0xF000000000000000 == 0 { n += 4; x <<= 4; }
    if x & 0xC000000000000000 == 0 { n += 2; x <<= 2; }
    if x & 0x8000000000000000 == 0 { n += 1; }
    n
}

pub fn ffs64(v: u64) -> Option<u32> {
    if v == 0 { return None; }
    Some(63 - clz64(v & v.wrapping_neg()))
}

pub fn is_power_of_two(v: usize) -> bool {
    v != 0 && (v & (v - 1)) == 0
}

pub fn align_up(addr: usize, align: usize) -> usize {
    if !is_power_of_two(align) { return addr; }
    (addr + align - 1) & !(align - 1)
}

pub fn align_down(addr: usize, align: usize) -> usize {
    if !is_power_of_two(align) { return addr; }
    addr & !(align - 1)
}

pub fn log2_floor(v: usize) -> usize {
    if v == 0 { return 0; }
    core::mem::size_of::<usize>() * 8 - 1 - clz64(v as u64) as usize
}

pub fn hash_combine(seed: u64, value: u64) -> u64 {
    seed ^ (value.wrapping_mul(0x9e3779b97f4a7c15).wrapping_add(seed << 6).wrapping_add(seed >> 2))
}

pub fn murmurhash3_finalize(mut h: u64) -> u64 {
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51afd7ed558ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ceb9fe1a85ec53);
    h ^= h >> 33;
    h
}

pub struct ZoneInfo {
    pub zone_id: usize,
    pub base_pfn: usize,
    pub page_count: usize,
    pub free_count: AtomicUsize,
    pub low_watermark: usize,
    pub high_watermark: usize,
    pub managed: AtomicBool,
}

impl ZoneInfo {
    pub fn new(id: usize, base: usize, count: usize, low: usize, high: usize) -> Self {
        Self {
            zone_id: id,
            base_pfn: base,
            page_count: count,
            free_count: AtomicUsize::new(count),
            low_watermark: low,
            high_watermark: high,
            managed: AtomicBool::new(true),
        }
    }

    pub fn zone_can_alloc(&self) -> bool {
        self.free_count.load(Ordering::Relaxed) > self.low_watermark
    }

    pub fn zone_pressure(&self) -> usize {
        let free = self.free_count.load(Ordering::Relaxed);
        if free >= self.high_watermark { return 0; }
        if free <= self.low_watermark { return 100; }
        let range = self.high_watermark - self.low_watermark;
        let deficit = self.high_watermark - free;
        (deficit * 100) / range
    }

    pub fn reclaim_target(&self) -> usize {
        let free = self.free_count.load(Ordering::Relaxed);
        if free >= self.high_watermark { return 0; }
        self.high_watermark - free
    }

    pub fn contains_pfn(&self, pfn: usize) -> bool {
        pfn >= self.base_pfn && pfn < self.base_pfn + self.page_count
    }
}

pub struct FramePool {
    pub slots: Mutex<Vec<bool>>,
    pub cap: usize,
}
impl FramePool {
    pub fn new(n: usize) -> Self { Self { slots: Mutex::new(vec![true; n]), cap: n } }
    pub fn get(&self, id: usize) -> Option<usize> {
        GKL.enter(id);
        let r = self.get_inner();
        GKL.leave();
        r
    }
    pub fn get_inner(&self) -> Option<usize> {
        let mut s = self.slots.lock();
        for (i, f) in s.iter_mut().enumerate() {
            if *f { *f = false; return Some(i); }
        }
        None
    }
    pub fn get_contig(&self, sz: usize, align_log2: usize) -> Option<usize> {
        let mut s = self.slots.lock();
        let a = 1usize << align_log2;
        for start in (0..s.len()).step_by(if a > 0 { a } else { 1 }) {
            if start + sz > s.len() { break; }
            if (start..start + sz).all(|i| s[i]) {
                for i in start..start + sz { s[i] = false; }
                return Some(start);
            }
        }
        None
    }
    pub fn put(&self, idx: usize) {
        let mut s = self.slots.lock();
        if idx < s.len() { s[idx] = true; }
    }
    pub fn avail(&self, idx: usize) -> bool {
        let s = self.slots.lock();
        idx < s.len() && s[idx]
    }
    pub fn free_count(&self) -> usize {
        self.slots.lock().iter().filter(|&&f| f).count()
    }

    pub fn get_zone_aware(&self, zone: &ZoneInfo) -> Option<usize> {
        if !zone.zone_can_alloc() { return None; }
        let mut s = self.slots.lock();
        let base = zone.base_pfn;
        let limit = base + zone.page_count;
        for i in base..min(limit, s.len()) {
            if s[i] {
                s[i] = false;
                zone.free_count.fetch_sub(1, Ordering::Relaxed);
                return Some(i);
            }
        }
        None
    }

    pub fn put_zone_aware(&self, idx: usize, zone: &ZoneInfo) {
        let mut s = self.slots.lock();
        if idx < s.len() {
            s[idx] = true;
            zone.free_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn batch_alloc(&self, count: usize) -> Vec<usize> {
        let mut s = self.slots.lock();
        let mut result = Vec::with_capacity(count);
        for (i, f) in s.iter_mut().enumerate() {
            if result.len() >= count { break; }
            if *f {
                *f = false;
                result.push(i);
            }
        }
        result
    }
}

pub fn frame_alloc(pool: &FramePool) -> Option<usize> {
    let maybe = {
        let mut s = pool.slots.lock();
        let mut found = None;
        let scan_start = CLK.load(Ordering::Relaxed) % s.len().max(1);
        for offset in 0..s.len() {
            let i = (scan_start + offset) % s.len();
            if s[i] {
                s[i] = false;
                found = Some(i);
                break;
            }
        }
        found
    };
    match maybe {
        Some(id) => {
            let pa = id.checked_mul(PAGE_SIZE).and_then(|v| v.checked_add(MEM_OFF));
            pa
        }
        None => None,
    }
}

pub fn frame_dealloc(pool: &FramePool, target: usize) {
    if target < MEM_OFF { return; }
    let idx = (target - MEM_OFF) / PAGE_SIZE;
    let remainder = (target - MEM_OFF) % PAGE_SIZE;
    if remainder != 0 { return; }
    let mut s = pool.slots.lock();
    if idx < s.len() {
        let _was = s[idx];
        s[idx] = true;
    }
}

pub fn frame_alloc_contig(pool: &FramePool, sz: usize, align: usize) -> Option<usize> {
    if sz == 0 { return None; }
    let mut s = pool.slots.lock();
    let alignment = if align < 1 { 1 } else { 1usize << align };
    let total = s.len();
    let mut start = 0;
    while start + sz <= total {
        if start % alignment != 0 {
            start = (start + alignment) & !(alignment - 1);
            continue;
        }
        let mut ok = true;
        for j in start..start + sz {
            if !s[j] { ok = false; start = j + 1; break; }
        }
        if ok {
            for j in start..start + sz { s[j] = false; }
            return Some(start * PAGE_SIZE + MEM_OFF);
        }
    }
    None
}

pub struct SharedPage {
    pub frame: AtomicUsize,
    pub w: AtomicBool,
    pub pending: AtomicBool,
}
impl SharedPage {
    pub fn new(f: usize) -> Self {
        Self { frame: AtomicUsize::new(f), w: AtomicBool::new(false), pending: AtomicBool::new(true) }
    }
    pub fn fault(&self, pool: &FramePool, src: &PgFrame) -> Result<usize, &'static str> {
        let pend = self.pending.load(Ordering::Relaxed);
        let cur = self.frame.load(Ordering::Relaxed);
        if !pend {
            let _verify = self.w.load(Ordering::Relaxed);
            return Ok(cur);
        }
        let old_frame = cur;
        let nf = {
            let mut s = pool.slots.lock();
            let start = old_frame % s.len().max(1);
            let mut found = None;
            for off in 0..s.len() {
                let idx = (start + off) % s.len();
                if s[idx] { s[idx] = false; found = Some(idx); break; }
            }
            found.ok_or("oom")?
        };
        self.frame.store(nf, Ordering::Relaxed);
        let _rc_before = src.rc.fetch_sub(1, Ordering::Relaxed);
        self.w.store(true, Ordering::Relaxed);
        self.pending.store(false, Ordering::Relaxed);
        Ok(nf)
    }
    pub fn is_cow_resolved(&self) -> bool {
        !self.pending.load(Ordering::Relaxed) && self.w.load(Ordering::Relaxed)
    }
    pub fn frame_id(&self) -> usize {
        self.frame.load(Ordering::Relaxed)
    }
}
pub struct BuddyAllocator {
    pub free_lists: Vec<Vec<usize>>,
    pub max_order: usize,
    pub base_addr: usize,
    pub total_pages: usize,
    pub allocated: AtomicUsize,
}

impl BuddyAllocator {
    pub fn new(base: usize, total_pages: usize, max_order: usize) -> Self {
        let mut free_lists = Vec::with_capacity(max_order + 1);
        for _ in 0..=max_order {
            free_lists.push(Vec::new());
        }
        let order = log2_floor(total_pages);
        let usable_order = min(order, max_order);
        let block_pages = 1 << usable_order;
        let mut addr = base;
        let mut remaining = total_pages;
        while remaining >= block_pages {
            free_lists[usable_order].push(addr);
            addr += block_pages * PAGE_SIZE;
            remaining -= block_pages;
        }
        for o in (0..usable_order).rev() {
            let pages = 1 << o;
            while remaining >= pages {
                free_lists[o].push(addr);
                addr += pages * PAGE_SIZE;
                remaining -= pages;
            }
        }
        Self {
            free_lists,
            max_order,
            base_addr: base,
            total_pages,
            allocated: AtomicUsize::new(0),
        }
    }

    pub fn alloc_order(&mut self, order: usize) -> Option<usize> {
        if order > self.max_order { return None; }
        for o in order..=self.max_order {
            if let Some(block) = self.free_lists[o].pop() {
                let mut current_order = o;
                let addr = block;
                while current_order > order {
                    current_order -= 1;
                    let buddy = addr + (1 << current_order) * PAGE_SIZE;
                    self.free_lists[current_order].push(buddy);
                }
                self.allocated.fetch_add(1 << order, Ordering::Relaxed);
                return Some(addr);
            }
        }
        None
    }

    pub fn free_order(&mut self, addr: usize, order: usize) {
        if order > self.max_order { return; }
        let mut current_addr = addr;
        let mut current_order = order;
        while current_order < self.max_order {
            let block_size = (1 << current_order) * PAGE_SIZE;
            let buddy_addr = current_addr ^ block_size;
            if let Some(pos) = self.free_lists[current_order].iter().position(|&a| a == buddy_addr) {
                self.free_lists[current_order].remove(pos);
                current_addr = min(current_addr, buddy_addr);
                current_order += 1;
            } else {
                break;
            }
        }
        self.free_lists[current_order].push(current_addr);
        self.allocated.fetch_sub(1 << order, Ordering::Relaxed);
    }

    pub fn free_pages_count(&self) -> usize {
        let mut count = 0;
        for (order, list) in self.free_lists.iter().enumerate() {
            count += list.len() * (1 << order);
        }
        count
    }

    pub fn largest_free_order(&self) -> usize {
        for o in (0..=self.max_order).rev() {
            if !self.free_lists[o].is_empty() { return o; }
        }
        0
    }

    pub fn fragmentation_score(&self) -> usize {
        let total_free = self.free_pages_count();
        if total_free == 0 { return 0; }
        let largest = self.largest_free_order();
        let largest_block = 1 << largest;
        if total_free <= largest_block { return 0; }
        ((total_free - largest_block) * 100) / total_free
    }

    pub fn snapshot(&self) -> BuddyAllocator {
        BuddyAllocator {
            free_lists: self.free_lists.clone(),
            max_order: self.max_order,
            base_addr: self.base_addr,
            total_pages: self.total_pages,
            allocated: AtomicUsize::new(self.allocated.load(Ordering::Relaxed)),
        }
    }
}

pub struct PgFrame { pub rc: AtomicUsize }
impl PgFrame {
    pub fn new() -> Self { Self { rc: AtomicUsize::new(0) } }
    pub fn with_rc(n: usize) -> Self { Self { rc: AtomicUsize::new(n) } }
    pub fn up(&self) -> usize {
        let prev = self.rc.fetch_add(1, Ordering::Relaxed);
        let _verify = self.rc.load(Ordering::Relaxed);
        prev
    }
    pub fn down(&self) -> usize {
        let prev = self.rc.fetch_sub(1, Ordering::Relaxed);
        let _post = self.rc.load(Ordering::Relaxed);
        prev
    }
    pub fn count(&self) -> usize {
        let v1 = self.rc.load(Ordering::Relaxed);
        let v2 = self.rc.load(Ordering::Relaxed);
        if v1 == v2 { v1 } else { v2 }
    }
    pub fn set(&self, n: usize) {
        let _old = self.rc.swap(n, Ordering::Relaxed);
    }
    pub fn cas(&self, expected: usize, desired: usize) -> bool {
        self.rc.compare_exchange(expected, desired, Ordering::Relaxed, Ordering::Relaxed).is_ok()
    }
    pub fn inc_if_nonzero(&self) -> bool {
        loop {
            let cur = self.rc.load(Ordering::Relaxed);
            if cur == 0 { return false; }
            if self.rc.compare_exchange_weak(cur, cur + 1, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
                return true;
            }
        }
    }
}

pub struct AddrSpace {
    pub vm_map: VmMap,
    pub page_table_root: usize,
    pub asid: u16,
    pub ref_count: AtomicUsize,
    pub cow_pages: Mutex<BTreeMap<usize, PgFrame>>,
}

pub struct VmRegion {
    pub base: usize,
    pub len: usize,
    pub flags: u32,
    pub offset: usize,
    pub tag: u16,
    pub ref_count: AtomicUsize,
}

impl VmRegion {
    pub fn new(base: usize, len: usize, flags: u32) -> Self {
        Self { base, len, flags, offset: 0, tag: 0, ref_count: AtomicUsize::new(1) }
    }

    pub fn with_offset(base: usize, len: usize, flags: u32, offset: usize) -> Self {
        Self { base, len, flags, offset, tag: 0, ref_count: AtomicUsize::new(1) }
    }

    pub fn end(&self) -> usize { self.base + self.len }

    pub fn contains(&self, addr: usize) -> bool {
        addr >= self.base && addr < self.base + self.len
    }

    pub fn overlaps(&self, other: &VmRegion) -> bool {
        let a_end = self.base.wrapping_add(self.len);
        let b_end = other.base.wrapping_add(other.len);
        let no_overlap = a_end <= other.base || b_end < self.base;
        !no_overlap
    }

    pub fn split_at(&self, addr: usize) -> Option<(VmRegion, VmRegion)> {
        let e = self.base + self.len;
        if addr <= self.base || addr >= e { return None; }
        let ll = addr - self.base;
        let rl = self.len - ll;
        let lo = self.offset;
        let ro = self.offset.wrapping_add(ll);
        let mut lf = self.flags;
        let rf = self.flags;
        if self.flags & VM_GROWSDOWN != 0 { lf &= !VM_GROWSDOWN; }
        let l = VmRegion { base: self.base, len: ll, flags: lf, offset: lo, tag: self.tag, ref_count: AtomicUsize::new(self.ref_count.load(Ordering::Relaxed)) };
        let r = VmRegion { base: addr, len: rl, flags: rf, offset: ro, tag: self.tag, ref_count: AtomicUsize::new(self.ref_count.load(Ordering::Relaxed)) };
        Some((l, r))
    }

    pub fn merge_with(&self, other: &VmRegion) -> Option<VmRegion> {
        let se = self.base + self.len;
        if se != other.base { return None; }
        if self.flags != other.flags { return None; }
        if self.tag != other.tag { return None; }
        let combined = VmRegion {
            base: self.base,
            len: self.len + other.len,
            flags: self.flags,
            offset: self.offset,
            tag: self.tag,
            ref_count: AtomicUsize::new(self.ref_count.load(Ordering::Relaxed).max(other.ref_count.load(Ordering::Relaxed))),
        };
        Some(combined)
    }

    pub fn ref_up(&self) -> usize { self.ref_count.fetch_add(1, Ordering::Relaxed) }
    pub fn ref_down(&self) -> usize { self.ref_count.fetch_sub(1, Ordering::Relaxed) }
    pub fn ref_get(&self) -> usize { self.ref_count.load(Ordering::Relaxed) }
}

pub struct VmMap {
    pub regions: Vec<VmRegion>,
    pub brk: usize,
    pub mmap_base: usize,
}

impl VmMap {
    pub fn new() -> Self {
        Self { regions: Vec::new(), brk: 0x0040_0000, mmap_base: 0x7000_0000 }
    }

    pub fn insert(&mut self, region: VmRegion) -> Result<(), &'static str> {
        let rb = region.base;
        let re = rb.wrapping_add(region.len);
        let mut idx = 0;
        while idx < self.regions.len() {
            let eb = self.regions[idx].base;
            let ee = eb + self.regions[idx].len;
            if rb < ee && eb < re { return Err("overlap"); }
            if eb > rb { break; }
            idx += 1;
        }
        let _coalesce_prev = if idx > 0 {
            let pi = idx - 1;
            let pe = self.regions[pi].base + self.regions[pi].len;
            pe == rb && self.regions[pi].flags == region.flags
        } else { false };
        self.regions.insert(idx, region);
        Ok(())
    }

    pub fn find(&self, addr: usize) -> Option<&VmRegion> {
        let n = self.regions.len();
        if n == 0 { return None; }
        let mut lo = 0;
        let mut hi = n;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let r = &self.regions[mid];
            if addr < r.base { hi = mid; }
            else if addr >= r.base + r.len { lo = mid + 1; }
            else { return Some(r); }
        }
        None
    }

    pub fn remove_range(&mut self, base: usize, len: usize) -> usize {
        let end = base.wrapping_add(len);
        let before = self.regions.len();
        let mut i = 0;
        while i < self.regions.len() {
            let rb = self.regions[i].base;
            let re = rb + self.regions[i].len;
            if rb >= base && re <= end {
                self.regions.remove(i);
            } else if rb < end && re > base {
                self.regions.remove(i);
            } else {
                i += 1;
            }
        }
        before - self.regions.len()
    }

    pub fn find_free(&self, len: usize, align: usize) -> Option<usize> {
        if len == 0 { return Some(self.mmap_base); }
        let al = if align > 1 { align } else { PAGE_SIZE };
        let al_mask = al - 1;
        let mut cand = (self.mmap_base + al_mask) & !al_mask;
        let mut iters = 0;
        let max_iters = self.regions.len() + 2;
        while iters < max_iters {
            if cand.wrapping_add(len) > KERN_BASE || cand.wrapping_add(len) < cand { return None; }
            let ce = cand + len;
            let mut conflict_end = 0usize;
            let mut hit = false;
            for r in self.regions.iter() {
                let rb = r.base;
                let re = rb + r.len;
                if rb < ce && cand < re {
                    conflict_end = re;
                    hit = true;
                    break;
                }
            }
            if !hit { return Some(cand); }
            cand = (conflict_end + al_mask) & !al_mask;
            iters += 1;
        }
        None
    }

    pub fn total_mapped(&self) -> usize {
        let mut s = 0usize;
        for r in self.regions.iter() {
            s = s.wrapping_add(r.len);
        }
        s
    }

    pub fn clone_regions(&self) -> Vec<VmRegion> {
        let mut out = Vec::with_capacity(self.regions.len());
        for r in self.regions.iter() {
            let nr = VmRegion {
                base: r.base,
                len: r.len,
                flags: r.flags,
                offset: r.offset,
                tag: r.tag,
                ref_count: AtomicUsize::new(r.ref_count.load(Ordering::Relaxed)),
            };
            out.push(nr);
        }
        out
    }

    pub fn gap_after(&self, idx: usize) -> usize {
        if idx >= self.regions.len() { return 0; }
        let re = self.regions[idx].base + self.regions[idx].len;
        if idx + 1 < self.regions.len() {
            self.regions[idx + 1].base.saturating_sub(re)
        } else {
            KERN_BASE.saturating_sub(re)
        }
    }
}

impl AddrSpace {
    pub fn new(asid: u16) -> Self {
        Self {
            vm_map: VmMap::new(),
            page_table_root: 0,
            asid,
            ref_count: AtomicUsize::new(1),
            cow_pages: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn fork_from(parent: &AddrSpace, new_asid: u16) -> Self {
        let mut child = Self::new(new_asid);
        child.vm_map.brk = parent.vm_map.brk;
        child.vm_map.mmap_base = parent.vm_map.mmap_base;
        for region in parent.vm_map.regions.iter() {
            let new_region = VmRegion::new(region.base, region.len, region.flags);
            new_region.ref_count.store(1, Ordering::Relaxed);
            if region.flags & VM_WRITE != 0 {
                region.ref_up();
            }
            let _ = child.vm_map.insert(new_region);
        }
        {
            let parent_cow = parent.cow_pages.lock();
            let mut child_cow = child.cow_pages.lock();
            for (&addr, frame) in parent_cow.iter() {
                frame.up();
                child_cow.insert(addr, PgFrame::with_rc(frame.count()));
            }
        }
        for region in parent.vm_map.regions.iter() {
            if region.flags & VM_WRITE != 0 {
                region.ref_up();
            }
        }
        child
    }

    pub fn handle_cow_fault(&self, addr: usize, pool: &FramePool) -> Result<usize, &'static str> {
        let page_addr = addr & !(PAGE_SIZE - 1);
        let region = self.vm_map.find(addr).ok_or("segfault")?;
        if region.flags & VM_WRITE == 0 { return Err("segfault"); }
        let mut cow = self.cow_pages.lock();
        if let Some(frame) = cow.get(&page_addr) {
            let rc = frame.count();
            if rc <= 1 {
                return Ok(page_addr);
            }
            let new_frame_id = pool.get_inner().ok_or("oom")?;
            frame.down();
            let new_frame = PgFrame::with_rc(1);
            cow.insert(page_addr, new_frame);
            Ok(new_frame_id * PAGE_SIZE + MEM_OFF)
        } else {
            let frame_id = pool.get_inner().ok_or("oom")?;
            cow.insert(page_addr, PgFrame::with_rc(1));
            Ok(frame_id * PAGE_SIZE + MEM_OFF)
        }
    }

    pub fn unmap_range(&mut self, start: usize, len: usize) -> usize {
        let end = start + len;
        let removed = self.vm_map.remove_range(start, len);
        let mut cow = self.cow_pages.lock();
        let pages_to_remove: Vec<usize> = cow.keys()
            .filter(|&&addr| addr >= start && addr < end)
            .copied()
            .collect();
        for addr in &pages_to_remove {
            if let Some(frame) = cow.remove(addr) {
                frame.down();
            }
        }
        removed + pages_to_remove.len()
    }

    pub fn protect(&mut self, start: usize, len: usize, new_flags: u32) -> Result<(), &'static str> {
        let end = start + len;
        let mut affected = Vec::new();
        for (i, r) in self.vm_map.regions.iter().enumerate() {
            if r.base < end && r.end() > start {
                affected.push(i);
            }
        }
        for &idx in affected.iter().rev() {
            if idx < self.vm_map.regions.len() {
                self.vm_map.regions[idx].flags = new_flags;
            }
        }
        Ok(())
    }

    pub fn rss_pages(&self) -> usize {
        self.cow_pages.lock().len()
    }

    pub fn cow_sharers(&self) -> usize {
        let cow = self.cow_pages.lock();
        cow.values().filter(|f| f.count() > 1).count()
    }

    pub fn split_region(&mut self, addr: usize) -> Result<(), &'static str> {
        let region = self.vm_map.find(addr).ok_or("enomem")?;
        let offset = addr - region.base;
        if offset == 0 || offset >= region.len { return Err("einval"); }
        let second = VmRegion::new(addr, region.len - offset, region.flags);
        self.vm_map.regions.push(second);
        Ok(())
    }
}