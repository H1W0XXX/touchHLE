/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! `NSConditionLock`.
//!
//! Unlike `NSLock`/`NSRecursiveLock` (see `ns_lock.rs`), this needs real
//! blocking wait-for-condition semantics, so instead of only using
//! `MutexState` directly, it drives the same guest-visible
//! `pthread_mutex_t`/`pthread_cond_t` machinery `libc`'s pthread
//! implementation uses (see `crate::libc::pthread`), just from host code
//! instead of guest code. That gets us real cooperative-scheduler blocking
//! (via `Environment::yield_thread`) for free.
//!
//! TODO: `lockBeforeDate:` and `lockWhenCondition:beforeDate:` (the
//! timed-wait variants) aren't implemented yet -- they currently behave the
//! same as the non-timed versions (wait forever rather than giving up after
//! the deadline).

use crate::frameworks::foundation::NSInteger;
use crate::libc::pthread::cond::{
    pthread_cond_broadcast, pthread_cond_init, pthread_cond_t, pthread_cond_wait,
};
use crate::libc::pthread::mutex::{
    pthread_mutex_destroy, pthread_mutex_init, pthread_mutex_lock, pthread_mutex_t,
    pthread_mutex_trylock, pthread_mutex_unlock,
};
use crate::mem::{guest_size_of, ConstPtr, MutPtr};
use crate::objc::{id, msg, nil, objc_classes, ClassExports, HostObject};

struct NSConditionLockHostObject {
    mutex: MutPtr<pthread_mutex_t>,
    cond: MutPtr<pthread_cond_t>,
    /// The current "condition" value. Only meaningful/safe to read while
    /// holding `mutex`, same as Apple's implementation.
    condition: NSInteger,
    name: id,
}
impl HostObject for NSConditionLockHostObject {}

pub const CLASSES: ClassExports = objc_classes! {

(env, this, _cmd);

@implementation NSConditionLock: NSObject

+ (id)alloc {
    log_dbg!("[NSConditionLock alloc]");
    let mutex: MutPtr<pthread_mutex_t> = env.mem.alloc(guest_size_of::<pthread_mutex_t>()).cast();
    assert_eq!(pthread_mutex_init(env, mutex, ConstPtr::null()), 0);
    let cond: MutPtr<pthread_cond_t> = env.mem.alloc(guest_size_of::<pthread_cond_t>()).cast();
    assert_eq!(pthread_cond_init(env, cond, ConstPtr::null()), 0);
    let host_object = NSConditionLockHostObject {
        mutex,
        cond,
        condition: 0,
        name: nil,
    };
    env.objc.alloc_object(this, Box::new(host_object), &mut env.mem)
}

- (id)init {
    // condition defaults to 0, already set by +alloc.
    this
}
- (id)initWithCondition:(NSInteger)condition {
    env.objc.borrow_mut::<NSConditionLockHostObject>(this).condition = condition;
    this
}

- (NSInteger)condition {
    // Apple's docs note this doesn't take the lock, it's inherently a bit
    // racy if another thread is concurrently changing the condition.
    env.objc.borrow::<NSConditionLockHostObject>(this).condition
}

// NSLocking protocol implementation
- (())lock {
    log_dbg!("[(NSConditionLock *){:?} lock]", this);
    let mutex = env.objc.borrow::<NSConditionLockHostObject>(this).mutex;
    assert_eq!(pthread_mutex_lock(env, mutex), 0);
}
- (())unlock {
    log_dbg!("[(NSConditionLock *){:?} unlock]", this);
    let mutex = env.objc.borrow::<NSConditionLockHostObject>(this).mutex;
    assert_eq!(pthread_mutex_unlock(env, mutex), 0);
}
- (bool)tryLock {
    let mutex = env.objc.borrow::<NSConditionLockHostObject>(this).mutex;
    pthread_mutex_trylock(env, mutex) == 0
}

- (())lockWhenCondition:(NSInteger)condition {
    log_dbg!("[(NSConditionLock *){:?} lockWhenCondition:{}]", this, condition);
    let (mutex, cond) = {
        let host_object = env.objc.borrow::<NSConditionLockHostObject>(this);
        (host_object.mutex, host_object.cond)
    };
    assert_eq!(pthread_mutex_lock(env, mutex), 0);
    loop {
        let current = env.objc.borrow::<NSConditionLockHostObject>(this).condition;
        if current == condition {
            break;
        }
        // pthread_cond_wait releases the mutex while blocked and
        // re-acquires it before returning.
        assert_eq!(pthread_cond_wait(env, cond, mutex), 0);
    }
    // Mutex remains locked, as documented.
}

- (bool)tryLockWhenCondition:(NSInteger)condition {
    let mutex = env.objc.borrow::<NSConditionLockHostObject>(this).mutex;
    if pthread_mutex_trylock(env, mutex) != 0 {
        return false;
    }
    let current = env.objc.borrow::<NSConditionLockHostObject>(this).condition;
    if current == condition {
        true // stays locked
    } else {
        assert_eq!(pthread_mutex_unlock(env, mutex), 0);
        false
    }
}

- (())unlockWithCondition:(NSInteger)newCondition {
    log_dbg!("[(NSConditionLock *){:?} unlockWithCondition:{}]", this, newCondition);
    let (mutex, cond) = {
        let host_object = env.objc.borrow_mut::<NSConditionLockHostObject>(this);
        host_object.condition = newCondition;
        (host_object.mutex, host_object.cond)
    };
    assert_eq!(pthread_cond_broadcast(env, cond), 0);
    assert_eq!(pthread_mutex_unlock(env, mutex), 0);
}

- (())setName:(id)name { // NSString *
    // @property(copy), name has to be copied
    let new_name: id = msg![env; name copy];
    env.objc.borrow_mut::<NSConditionLockHostObject>(this).name = new_name;
}
- (id)name {
    env.objc.borrow::<NSConditionLockHostObject>(this).name
}

- (())dealloc {
    log_dbg!("[(NSConditionLock *){:?} dealloc]", this);
    let (mutex, cond) = {
        let host_object = env.objc.borrow::<NSConditionLockHostObject>(this);
        (host_object.mutex, host_object.cond)
    };
    // Deliberately not calling pthread_cond_destroy: it asserts no thread is
    // still waiting on the condition variable, which would turn a guest bug
    // (e.g. deallocating a lock while another thread is blocked on it) into
    // a touchHLE panic instead of merely undefined behaviour, as it would be
    // on a real system.
    pthread_mutex_destroy(env, mutex);
    env.mem.free(mutex.cast_void());
    env.mem.free(cond.cast_void());
    env.objc.dealloc_object(this, &mut env.mem)
}

@end

};
