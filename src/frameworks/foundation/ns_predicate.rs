/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! `NSPredicate`.

use crate::objc::{
    autorelease, id, msg, msg_class, nil, objc_classes, release, retain, ClassExports, HostObject,
    NSZonePtr,
};

struct NSPredicateHostObject {
    /// `NSString *`
    format: id,
}
impl HostObject for NSPredicateHostObject {}

pub const CLASSES: ClassExports = objc_classes! {

(env, this, _cmd);

@implementation NSPredicate: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::new(NSPredicateHostObject { format: nil });
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

+ (id)predicateWithFormat:(id)format, ..._args {
    let predicate: id = msg_class![env; NSPredicate alloc];
    let predicate: id = msg![env; predicate initWithFormat:format];
    autorelease(env, predicate)
}

- (id)initWithFormat:(id)format {
    retain(env, format);
    env.objc.borrow_mut::<NSPredicateHostObject>(this).format = format;
    this
}

- (id)predicateFormat {
    env.objc.borrow::<NSPredicateHostObject>(this).format
}

- (bool)evaluateWithObject:(id)_object {
    // Enough for games that use predicates to filter local config arrays.
    true
}

- (())dealloc {
    let host_object = env.objc.borrow_mut::<NSPredicateHostObject>(this);
    let format = std::mem::replace(&mut host_object.format, nil);
    release(env, format);
    env.objc.dealloc_object(this, &mut env.mem)
}

@end

};
