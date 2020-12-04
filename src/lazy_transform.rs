use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::{Acquire, AcqRel, Relaxed, Release};

use crossbeam::epoch::{self, Atomic, Guard, Shared, Owned};

// from https://morestina.net/blog/742/exploring-lock-free-rust-1-locks (updated version)

#[derive(Debug)]
pub struct LazyTransform<T, S, FN> {
    transform_fn: FN,
    source: Atomic<ManuallyDrop<S>>,
    value: Atomic<T>,
    transform_lock: LightLock,
}

impl<T: Clone, S, FN: Fn(S) -> Option<T>> LazyTransform<T, S, FN> {
    pub fn new(transform_fn: FN) -> LazyTransform<T, S, FN> {
        LazyTransform {
            transform_fn: transform_fn,
            source: Atomic::null(),
            value: Atomic::null(),
            transform_lock: LightLock::new(),
        }
    }

    // Publish a new source.
    pub fn set_source(&self, source: S) {
        let guard = epoch::pin();
        let source_ptr = Owned::new(ManuallyDrop::new(source));
        let prev = self.source.swap(source_ptr, AcqRel, &guard);
        if !prev.is_null() {
            unsafe { guard.defer_destroy(prev); }
        }
    }

    // Transform and drop the newly published SOURCE if available.  Caches the
    // new value and returns a copy.  Returns None if no new source exists, if
    // the lock is already taken, or if transformation fails.
    fn try_transform(&self, guard: &Guard) -> Option<T> {
        if let Some(_lock_guard) = self.transform_lock.try_lock() {
            let source = self.source.swap(Shared::null(), AcqRel, &guard);
            if source.is_null() {
                return None;
            }
            let source_data;
            unsafe {
                guard.defer_destroy(source);
                source_data = ManuallyDrop::into_inner(ptr::read(source.as_raw()));
            }
            let newval = match (self.transform_fn)(source_data) {
                Some(newval) => newval,
                None => return None,
            };
            let prev = self.value.swap(
                Owned::new(newval.clone()),
                AcqRel,
                &guard,
            );
            unsafe {
                if !prev.is_null() {
                    guard.defer_destroy(prev);
                }
            }
            return Some(newval);
        }
        None
    }

    // Lazily generate a new value if a new source is provided.  Otherwise,
    // return the cached value.
    pub fn get_transformed(&self) -> Option<T> {
        let guard = epoch::pin();
        let source = self.source.load(Relaxed, &guard);
        if !source.is_null() {
            let newval = self.try_transform(&guard);
            if newval.is_some() {
                return newval;
            }
        }
        unsafe {
            self.value
                .load(Acquire, &guard)
                .as_ref()
                .map(T::clone)
        }
    }
}

#[derive(Debug)]
struct LightLock(AtomicBool);

impl LightLock {
    pub fn new() -> LightLock {
        LightLock(AtomicBool::new(false))
    }

    pub fn try_lock<'a>(&'a self) -> Option<LightGuard<'a>> {
        let was_locked = self.0.swap(true, Acquire);
        if was_locked {
            None
        } else {
            Some(LightGuard { lock: self })
        }
    }
}

struct LightGuard<'a> {
    lock: &'a LightLock,
}

impl<'a> Drop for LightGuard<'a> {
    fn drop(&mut self) {
        self.lock.0.store(false, Release);
    }
}
