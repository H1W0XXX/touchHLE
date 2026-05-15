/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! Handling of Objective-C messaging (`objc_msgSend` and friends).
//!
//! Resources:
//! - Apple's [Objective-C Runtime Programming Guide](https://developer.apple.com/library/archive/documentation/Cocoa/Conceptual/ObjCRuntimeGuide/Articles/ocrtHowMessagingWorks.html)
//! - [Apple's documentation of `objc_msgSend`](https://developer.apple.com/documentation/objectivec/1456712-objc_msgsend)
//! - Mike Ash's [objc_msgSend's New Prototype](https://www.mikeash.com/pyblog/objc_msgsends-new-prototype.html)
//! - Peter Steinberger's [Calling Super at Runtime in Swift](https://steipete.com/posts/calling-super-at-runtime/) explains `objc_msgSendSuper2`

use super::{id, nil, Class, ObjC, IMP, SEL};
use crate::abi::{CallFromHost, GuestRet};
use crate::environment::ThreadId;
use crate::frameworks::core_graphics::{CGPoint, CGSize};
use crate::frameworks::foundation::{ns_property_list_serialization, ns_string};
use crate::libc::pthread::cond::{
    pthread_cond_broadcast, pthread_cond_destroy, pthread_cond_init, pthread_cond_t,
    pthread_cond_wait,
};
use crate::libc::pthread::mutex::{
    pthread_mutex_destroy, pthread_mutex_init, pthread_mutex_lock, pthread_mutex_t,
    pthread_mutex_unlock,
};
use crate::mem::{guest_size_of, ConstPtr, MutPtr, MutVoidPtr, SafeRead};
use crate::objc::classes::InitializationStatus;
use crate::Environment;
use std::any::TypeId;

pub(super) struct ThreadInitializer {
    mutex: MutPtr<pthread_mutex_t>,
    cond: MutPtr<pthread_cond_t>,
    tid: ThreadId,
    waiters: u32,
}

fn maybe_initialize_class(env: &mut Environment, receiver: id) {
    let Some(class_host_object) = env.objc.get_host_object(receiver) else {
        return;
    };
    let Some(&super::ClassHostObject {
        superclass,
        is_metaclass,
        is_initialized,
        ..
    }) = class_host_object.as_any().downcast_ref()
    else {
        // If it's here, there's one of two cases:
        //
        // 1: The receiver is an instance. The class should then have already
        // called +initialize since you need to call +alloc to create an
        // instance (this also needs to be upheld for instances created with
        // class_createInstance(), whenever we implement that)
        //
        // 2: The reciever is a fake/unimplemented class. There's no reason to
        // send +initialize to those, so we don't bother.
        return;
    };

    if is_metaclass || is_initialized == InitializationStatus::Initialized {
        // On the offchance that this is a metaclass, we don't need to send
        // +initialize to it. We also don't need to send it if the class is
        // already initialized.
        return;
    }

    // This class is not initialized, but there might be classes above it in the
    // hierarchy that also need to be checked, so check those first.
    if !superclass.is_null() {
        maybe_initialize_class(env, superclass);
    }

    if is_initialized == InitializationStatus::Initializing {
        env.objc
            .initializer_threads
            .get_mut(&receiver)
            .unwrap()
            .waiters += 1;
        let ThreadInitializer {
            mutex, cond, tid, ..
        } = *env.objc.initializer_threads.get(&receiver).unwrap();

        // The current thread is already initializing, so let it call other
        // messages while it does so.
        if tid == env.current_thread {
            return;
        }

        // We are waiting for another thread to initialize, wait for it to
        // broadcast that it has finished.
        pthread_mutex_lock(env, mutex);
        loop {
            let class_host_object = env.objc.get_host_object(receiver).unwrap();
            let &super::ClassHostObject { is_initialized, .. } =
                class_host_object.as_any().downcast_ref().unwrap();
            if is_initialized == InitializationStatus::Initialized {
                break;
            }
            pthread_cond_wait(env, cond, mutex);
        }
        pthread_mutex_unlock(env, mutex);

        let ThreadInitializer {
            ref mut waiters, ..
        } = *env.objc.initializer_threads.get_mut(&receiver).unwrap();
        *waiters -= 1;
        if *waiters == 0 {
            // We're the last waiter for this initialize, so clean up state on
            // the way out.
            pthread_cond_destroy(env, cond);
            pthread_mutex_destroy(env, mutex);
            env.objc.initializer_threads.remove(&receiver);
        }
    } else {
        log_dbg!(
            "Initializing {:?} on thread {}",
            env.objc.try_get_class_name(receiver),
            env.current_thread
        );
        let regs = *env.cpu.regs();

        let mutex = env.mem.alloc(guest_size_of::<pthread_mutex_t>()).cast();
        let cond = env.mem.alloc(guest_size_of::<pthread_cond_t>()).cast();
        pthread_mutex_init(env, mutex, ConstPtr::null());
        pthread_cond_init(env, cond, ConstPtr::null());
        env.objc.initializer_threads.insert(
            receiver,
            ThreadInitializer {
                mutex,
                cond,
                tid: env.current_thread,
                waiters: 0,
            },
        );

        let super::ClassHostObject { is_initialized, .. } = env.objc.borrow_mut(receiver);
        *is_initialized = InitializationStatus::Initializing;
        () = msg![env; receiver initialize];
        let super::ClassHostObject { is_initialized, .. } = env.objc.borrow_mut(receiver);
        *is_initialized = InitializationStatus::Initialized;
        env.cpu.regs_mut().copy_from_slice(&regs);
        log_dbg!(
            "Done initializing {:?} on thread {}",
            env.objc.try_get_class_name(receiver),
            env.current_thread
        );
        if env.objc.initializer_threads.get(&receiver).unwrap().waiters == 0 {
            // Nobody ended up waiting for this initializer, so we can just
            // destroy it.
            pthread_cond_destroy(env, cond);
            pthread_mutex_destroy(env, mutex);
            env.objc.initializer_threads.remove(&receiver);
        } else {
            pthread_mutex_lock(env, mutex);
            pthread_cond_broadcast(env, cond);
            pthread_mutex_unlock(env, mutex);
        }
    }
}

fn trace_zombie_farm_status_message(class_name: &str, selector_name: &str) -> bool {
    let _ = (class_name, selector_name);
    false
}

fn trace_zombie_farm_layout_message(class_name: &str, selector_name: &str) -> bool {
    let table_delegate_selector = matches!(
        selector_name,
        "table:cellAtIndex:" | "numberOfCellsInTable:" | "cellClassForTable:"
    );
    let interesting_class = table_delegate_selector
        || class_name.contains("TableView")
        || class_name == "CCScrollView"
        || class_name.ends_with("Cell");
    let interesting_selector = matches!(
        selector_name,
        "setContentSize:"
            | "setViewSize:"
            | "setContentOffset:"
            | "setDirection:"
            | "direction"
            | "contentSize"
            | "viewSize"
            | "contentOffset"
            | "cellSize"
            | "_setIndex:forCell:"
            | "_indexFromOffset:"
            | "_offsetFromIndex:"
            | "dequeueCell"
            | "_addCellIfNecessary:"
            | "_moveCellOutOfSight:"
            | "_evictCell"
            | "cellWithIndex:"
            | "table:cellAtIndex:"
            | "numberOfCellsInTable:"
            | "cellClassForTable:"
            | "scrollViewDidScroll:"
    );
    interesting_class && interesting_selector
}

fn trace_zombie_farm_layout_to_console(selector_name: &str) -> bool {
    !matches!(
        selector_name,
        "setContentSize:"
            | "setViewSize:"
            | "setContentOffset:"
            | "direction"
            | "contentSize"
            | "viewSize"
            | "contentOffset"
            | "cellSize"
            | "_setIndex:forCell:"
            | "_indexFromOffset:"
            | "_offsetFromIndex:"
            | "dequeueCell"
            | "_addCellIfNecessary:"
            | "_moveCellOutOfSight:"
            | "_evictCell"
            | "cellWithIndex:"
            | "table:cellAtIndex:"
            | "numberOfCellsInTable:"
            | "cellClassForTable:"
            | "scrollViewDidScroll:"
    )
}

fn point_arg_from_regs(regs: &[u32], start: usize) -> CGPoint {
    CGPoint {
        x: f32::from_bits(regs[start]),
        y: f32::from_bits(regs[start + 1]),
    }
}

fn size_arg_from_regs(regs: &[u32], start: usize) -> CGSize {
    CGSize {
        width: f32::from_bits(regs[start]),
        height: f32::from_bits(regs[start + 1]),
    }
}

fn zombie_farm_layout_arg_details(selector_name: &str, regs: &[u32]) -> Option<String> {
    match selector_name {
        "setContentSize:" | "setViewSize:" => {
            Some(format!("arg size={}", size_arg_from_regs(regs, 2)))
        }
        "setPosition:" | "setContentOffset:" => {
            Some(format!("arg point={}", point_arg_from_regs(regs, 2)))
        }
        "setDirection:" => Some(format!("arg value={}", regs[2])),
        "_offsetFromIndex:" => Some(format!("arg index={}", regs[3])),
        "cellWithIndex:" => Some(format!("arg index={}", regs[2])),
        "_addCellIfNecessary:" | "_moveCellOutOfSight:" => {
            Some(format!("arg cell={:?}", id::from_bits(regs[2])))
        }
        "table:cellAtIndex:" => Some(format!(
            "arg table={:?} index={}",
            id::from_bits(regs[2]),
            regs[3]
        )),
        "numberOfCellsInTable:" | "cellClassForTable:" => {
            Some(format!("arg table={:?}", id::from_bits(regs[2])))
        }
        "_indexFromOffset:" => Some(format!("arg point={}", point_arg_from_regs(regs, 2))),
        "scrollViewDidScroll:" => Some(format!("arg object={:?}", id::from_bits(regs[2]))),
        "setCellLayer:" | "setCellLayer2:" | "setCellActor:" => {
            Some(format!("arg object={:?}", id::from_bits(regs[2])))
        }
        "setCellID:" | "setCurrentCellIndex:" => Some(format!("arg value={}", regs[2])),
        _ => None,
    }
}

fn zombie_farm_md5sum_override(env: &mut Environment, selector_name: &str) -> Option<id> {
    if selector_name != "md5sum:"
        || !(env
            .bundle
            .bundle_identifier()
            .starts_with("com.playforge.ZombieFarm")
            || env
                .bundle
                .bundle_identifier()
                .starts_with("com.playforge.ZFR"))
    {
        return None;
    }

    let path = id::from_bits(env.cpu.regs()[2]);
    if path == nil {
        return None;
    }

    let class = ObjC::read_isa(path, &env.mem);
    if class == nil {
        return None;
    }
    let string_class = env.objc.get_known_class("NSString", &mut env.mem);
    if !env.objc.class_is_subclass_of(class, string_class) {
        return None;
    }

    let path = ns_string::to_rust_string(env, path).to_string();
    let digest = ns_property_list_serialization::zombie_farm_expected_plist_md5_hex(env, &path)?;
    log_dbg!(
        "ZombieFarm: using digest.plist MD5 for {}: {}",
        path,
        digest
    );
    let digest = ns_string::from_rust_string(env, digest);
    Some(autorelease(env, digest))
}

fn trace_zombie_farm_layout_stret_return(
    env: &mut Environment,
    receiver: id,
    selector: SEL,
    stret: MutVoidPtr,
) {
    if !env
        .bundle
        .bundle_identifier()
        .starts_with("com.playforge.Z")
        || stret.is_null()
    {
        return;
    }
    if receiver == nil {
        return;
    }
    let selector_name = selector.as_str(&env.mem);
    let class = ObjC::read_isa(receiver, &env.mem);
    if class == nil {
        return;
    }
    let Some(class_name) = env.objc.try_get_class_name(class) else {
        return;
    };
    if !trace_zombie_farm_layout_message(class_name, selector_name) {
        return;
    }
    match selector_name {
        "cellSize" | "viewSize" | "contentSize" => {
            let size: CGSize = env.mem.read(stret.cast());
            crate::zombie_farm_debug::record_table_size_return(
                receiver,
                class_name,
                selector_name,
                size,
            );
            if trace_zombie_farm_layout_to_console(selector_name) {
                log_dbg!(
                    "ZombieFarm trace: [{} {}] receiver {:?} return size={}",
                    class_name,
                    selector_name,
                    receiver,
                    size
                );
            }
        }
        "position" | "contentOffset" => {
            let point: CGPoint = env.mem.read(stret.cast());
            crate::zombie_farm_debug::record_table_point_return(
                receiver,
                class_name,
                selector_name,
                point,
            );
            if trace_zombie_farm_layout_to_console(selector_name) {
                log_dbg!(
                    "ZombieFarm trace: [{} {}] receiver {:?} return point={}",
                    class_name,
                    selector_name,
                    receiver,
                    point
                );
            }
        }
        _ => {}
    }
}

fn trace_zombie_farm_layout_normal_return(env: &mut Environment, receiver: id, selector: SEL) {
    if !env
        .bundle
        .bundle_identifier()
        .starts_with("com.playforge.Z")
        || receiver == nil
    {
        return;
    }
    let selector_name = selector.as_str(&env.mem);
    let class = ObjC::read_isa(receiver, &env.mem);
    if class == nil {
        return;
    }
    let Some(class_name) = env.objc.try_get_class_name(class) else {
        return;
    };
    if !trace_zombie_farm_layout_message(class_name, selector_name) {
        return;
    }
    match selector_name {
        "direction" | "_indexFromOffset:" => {
            crate::zombie_farm_debug::record_table_value_return(
                receiver,
                class_name,
                selector_name,
                env.cpu.regs()[0],
            );
            log_dbg!(
                "ZombieFarm trace: [{} {}] receiver {:?} return value={}",
                class_name,
                selector_name,
                receiver,
                env.cpu.regs()[0]
            );
        }
        "cellWithIndex:" | "table:cellAtIndex:" | "cellClassForTable:" | "dequeueCell" => {
            let object = id::from_bits(env.cpu.regs()[0]);
            let object_class_name = if object == nil {
                None
            } else {
                let object_class = ObjC::read_isa(object, &env.mem);
                if object_class == nil {
                    None
                } else {
                    env.objc.try_get_class_name(object_class)
                }
            };
            crate::zombie_farm_debug::record_table_object_return(
                receiver,
                class_name,
                selector_name,
                object,
                object_class_name,
            );
            if trace_zombie_farm_layout_to_console(selector_name) {
                log_dbg!(
                    "ZombieFarm trace: [{} {}] receiver {:?} return object {:?} ({:?})",
                    class_name,
                    selector_name,
                    receiver,
                    object,
                    object_class_name
                );
            }
        }
        _ => {}
    }
}

fn zombie_farm_disable_cctable_cell_reuse(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if selector_name != "dequeueCell"
        || receiver == nil
        || !env
            .bundle
            .bundle_identifier()
            .starts_with("com.playforge.Z")
    {
        return false;
    }

    let class = ObjC::read_isa(receiver, &env.mem);
    let Some(class_name) = env.objc.try_get_class_name(class) else {
        return false;
    };
    if !class_name.contains("TableView") {
        return false;
    }

    crate::zombie_farm_debug::record_table_object_return(
        receiver,
        class_name,
        selector_name,
        nil,
        None,
    );
    crate::zombie_farm_debug::record_layout_event(format!(
        "[0x{:x} {} dequeueCell] forced nil; Zombie Farm table reuse disabled",
        receiver.to_bits(),
        class_name
    ));
    env.cpu.regs_mut()[0] = nil.to_bits();
    true
}

fn zombie_farm_cell_content_size_override(
    env: &mut Environment,
    receiver: id,
    selector: SEL,
    stret: MutVoidPtr,
) -> bool {
    if !env
        .bundle
        .bundle_identifier()
        .starts_with("com.playforge.Z")
        || receiver == nil
        || stret.is_null()
        || selector.as_str(&env.mem) != "contentSize"
    {
        return false;
    }

    let class = ObjC::read_isa(receiver, &env.mem);
    if class == nil {
        return false;
    }

    let Some(class_name) = env.objc.try_get_class_name(class) else {
        return false;
    };
    if !(class_name.starts_with("ZF") && class_name.ends_with("Cell")) {
        return false;
    }
    let class_name = class_name.to_string();

    let Some(cell_size_selector) = env.objc.lookup_selector("cellSize") else {
        return false;
    };
    if !env
        .objc
        .object_has_method(&env.mem, class, cell_size_selector)
    {
        return false;
    }

    let cell_size: CGSize = msg_send_no_type_checking(env, (class, cell_size_selector));
    env.mem.write(stret.cast(), cell_size);
    log_dbg!(
        "ZombieFarm workaround: [{} contentSize] -> class cellSize {}",
        class_name,
        cell_size
    );
    true
}

fn zombie_farm_prepare_cctable_cell(env: &mut Environment, receiver: id, selector: SEL) {
    let is_set_index = selector.as_str(&env.mem) == "_setIndex:forCell:";
    if !env
        .bundle
        .bundle_identifier()
        .starts_with("com.playforge.Z")
        || !is_set_index
    {
        return;
    }

    let table_class = ObjC::read_isa(receiver, &env.mem);
    let Some(table_class_name) = env.objc.try_get_class_name(table_class) else {
        return;
    };
    if !table_class_name.contains("TableView") {
        return;
    }

    let regs = env.cpu.regs();
    let index = regs[2];
    let cell = id::from_bits(regs[3]);
    if cell == nil {
        return;
    }

    let cell_class = ObjC::read_isa(cell, &env.mem);
    if cell_class == nil {
        return;
    }
    let Some(cell_class_name) = env.objc.try_get_class_name(cell_class) else {
        return;
    };
    let cell_class_name = cell_class_name.to_string();

    let Some(cell_size_selector) = env.objc.lookup_selector("cellSize") else {
        return;
    };
    if !env
        .objc
        .object_has_method(&env.mem, cell_class, cell_size_selector)
    {
        return;
    }
    let Some(set_content_size_selector) = env.objc.lookup_selector("setContentSize:") else {
        return;
    };

    let cell_size: CGSize = msg_send_no_type_checking(env, (cell_class, cell_size_selector));

    if env
        .objc
        .object_has_method(&env.mem, cell_class, set_content_size_selector)
    {
        let _: () = msg_send_no_type_checking(env, (cell, set_content_size_selector, cell_size));
    }

    let node = if let Some(node_selector) = env.objc.lookup_selector("node") {
        if env.objc.object_has_method(&env.mem, cell, node_selector) {
            msg_send_no_type_checking(env, (cell, node_selector))
        } else {
            nil
        }
    } else {
        nil
    };
    if node != nil {
        let node_class = ObjC::read_isa(node, &env.mem);
        if node_class != nil
            && env
                .objc
                .object_has_method(&env.mem, node_class, set_content_size_selector)
        {
            let _: () =
                msg_send_no_type_checking(env, (node, set_content_size_selector, cell_size));
        }
    }

    log_dbg!(
        "ZombieFarm workaround: prepared {} index {} cell {:?} node {:?} contentSize={}",
        cell_class_name,
        index,
        cell,
        node,
        cell_size
    );
}

/// The core implementation of `objc_msgSend`, the main function of Objective-C.
///
/// Note that while only two parameters (usually receiver and selector) are
/// defined by the wrappers over this function, a call to an `objc_msgSend`
/// variant may have additional arguments to be forwarded (or rather, left
/// untouched) by `objc_msgSend` when it tail-calls the method implementation it
/// looks up. This is invisible to the Rust type system; we're relying on
/// [crate::abi::CallFromGuest] here.
///
/// Similarly, the return value of `objc_msgSend` is whatever value is returned
/// by the method implementation. We are relying on CallFromGuest not
/// overwriting it.
#[allow(non_snake_case)]
fn objc_msgSend_inner(
    env: &mut Environment,
    receiver: id,
    selector: SEL,
    super2: Option<Class>,
    tolerate_type_mismatch: bool,
) {
    log_dbg!(
        "Dispatching {} for {:?}",
        selector.as_str(&env.mem),
        receiver
    );
    let message_type_info = env.objc.message_type_info.take();

    if receiver == nil {
        // https://developer.apple.com/library/archive/documentation/Cocoa/Conceptual/ObjectiveC/Chapters/ocObjectsClasses.html#//apple_ref/doc/uid/TP30001163-CH11-SW7
        log_dbg!("[nil {}]", selector.as_str(&env.mem));
        env.cpu.regs_mut()[0..2].fill(0);
        return;
    }

    let orig_class = super2.unwrap_or_else(|| ObjC::read_isa(receiver, &env.mem));
    if orig_class == nil {
        let selector_name = selector.as_str(&env.mem);
        if matches!(selector_name, "release" | "retain" | "autorelease") {
            log!(
                "Warning: ignoring {} sent to object {:?} with nil isa",
                selector_name,
                receiver
            );
            return;
        }
        panic!(
            "Receiver {:?} for selector \"{}\" has nil isa",
            receiver, selector_name
        );
    }
    maybe_initialize_class(env, receiver);

    let selector_name = selector.as_str(&env.mem).to_string();
    if let Some(result) = zombie_farm_md5sum_override(env, &selector_name) {
        env.cpu.regs_mut()[0] = result.to_bits();
        return;
    }
    if zombie_farm_disable_cctable_cell_reuse(env, receiver, &selector_name) {
        return;
    }
    let regs_before_zombie_farm_prepare = *env.cpu.regs();
    zombie_farm_prepare_cctable_cell(env, receiver, selector);
    env.cpu
        .regs_mut()
        .copy_from_slice(&regs_before_zombie_farm_prepare);

    // Traverse the chain of superclasses to find the method implementation.

    let mut class = orig_class;
    loop {
        if class == nil {
            assert!(class != orig_class);

            let class_host_object = env.objc.get_host_object(orig_class).unwrap();
            let &super::ClassHostObject {
                ref name,
                is_metaclass,
                ..
            } = class_host_object.as_any().downcast_ref().unwrap();

            panic!(
                "{} {:?} ({}class \"{}\", {:?}){} does not respond to selector \"{}\"!",
                if is_metaclass { "Class" } else { "Object" },
                receiver,
                if is_metaclass { "meta" } else { "" },
                name,
                orig_class,
                if super2.is_some() {
                    "'s superclass"
                } else {
                    ""
                },
                selector.as_str(&env.mem),
            );
        }

        let Some(host_object) = env.objc.get_host_object(class) else {
            let selector_name = selector.as_str(&env.mem);
            if matches!(selector_name, "release" | "retain" | "autorelease") {
                log!(
                    "Warning: ignoring {} sent to object {:?} with unregistered class {:?}",
                    selector_name,
                    receiver,
                    class
                );
                if selector_name == "retain" || selector_name == "autorelease" {
                    env.cpu.regs_mut()[0] = receiver.to_bits();
                }
                return;
            }
            panic!(
                "Receiver {:?} for selector \"{}\" has unregistered class {:?}",
                receiver, selector_name, class
            );
        };

        if let Some(&super::ClassHostObject {
            superclass,
            ref methods,
            ref name,
            ..
        }) = host_object.as_any().downcast_ref()
        {
            // Skip method lookup on first iteration if this is the super-call
            // variant of objc_msgSend (look up the superclass first)
            if super2.is_some() && class == orig_class {
                class = superclass;
                continue;
            }

            if let Some(imp) = methods.get(&selector) {
                log_dbg!("Found method on: {}", name);
                let selector_name = selector.as_str(&env.mem);
                let receiver_class_name = env.objc.try_get_class_name(orig_class).unwrap_or(name);
                let trace_zombie_farm_layout =
                    trace_zombie_farm_layout_message(receiver_class_name, selector_name)
                        || trace_zombie_farm_layout_message(name, selector_name);
                if trace_zombie_farm_status_message(receiver_class_name, selector_name)
                    || trace_zombie_farm_status_message(name, selector_name)
                    || trace_zombie_farm_layout
                {
                    let imp_description = match imp {
                        IMP::Host(_) => "host".to_string(),
                        IMP::Guest(guest_imp) => format!("{:?}", guest_imp),
                    };
                    let arg_description =
                        zombie_farm_layout_arg_details(selector_name, env.cpu.regs())
                            .map(|arg| format!(" {}", arg))
                            .unwrap_or_default();
                    crate::zombie_farm_debug::record_table_args(
                        receiver,
                        receiver_class_name,
                        selector_name,
                        env.cpu.regs(),
                    );
                    crate::zombie_farm_debug::record_layout_event(format!(
                        "[0x{:x} {} {}]{}",
                        receiver.to_bits(),
                        receiver_class_name,
                        selector_name,
                        arg_description
                    ));
                    if trace_zombie_farm_layout_to_console(selector_name) {
                        log_dbg!(
                            "ZombieFarm trace: [{} {}] receiver {:?}, implementation class {}, imp {}{}",
                            receiver_class_name,
                            selector_name,
                            receiver,
                            name,
                            imp_description,
                            arg_description,
                        );
                    }
                }
                match imp {
                    IMP::Host(host_imp) => {
                        // TODO: do type checks when calling GuestIMPs too.
                        // That requires using Objective-C type strings,
                        // rather than Rust types, and should probably
                        // warn rather than panicking,
                        // because apps might rely on type punning.
                        if let Some((sent_type_id, sent_type_desc)) = message_type_info {
                            let (expected_type_id, expected_type_desc) = host_imp.type_info();
                            if sent_type_id != expected_type_id {
                                let msg = format!(
                                    "\
Type mismatch when sending message {} to {:?}!
- Message has type: {:?} / {}
- Method expects type: {:?} / {}",
                                    selector.as_str(&env.mem),
                                    receiver,
                                    sent_type_id,
                                    sent_type_desc,
                                    expected_type_id,
                                    expected_type_desc
                                );
                                if tolerate_type_mismatch {
                                    log!("Warning: {}", msg);
                                } else {
                                    panic!("{}", msg);
                                }
                            }
                        }
                        host_imp.call_from_guest(env)
                    }
                    // We can't create a new stack frame, because that would
                    // interfere with pass-through of stack arguments.
                    IMP::Guest(guest_imp) => guest_imp.call_without_pushing_stack_frame(env),
                }
                if trace_zombie_farm_layout {
                    trace_zombie_farm_layout_normal_return(env, receiver, selector);
                }
                return;
            } else {
                class = superclass;
            }
        } else if let Some(&super::UnimplementedClass {
            ref name,
            is_metaclass,
        }) = host_object.as_any().downcast_ref()
        {
            panic!(
                "Class \"{}\" ({:?}) is unimplemented. Call to {} method \"{}\".",
                name,
                class,
                if is_metaclass { "class" } else { "instance" },
                selector.as_str(&env.mem),
            );
        } else if let Some(&super::FakeClass {
            ref name,
            is_metaclass,
        }) = host_object.as_any().downcast_ref()
        {
            log_dbg!(
                "Call to faked class \"{}\" ({:?}) {} method \"{}\". Behaving as if message was sent to nil.",
                name,
                class,
                if is_metaclass { "class" } else { "instance" },
                selector.as_str(&env.mem),
            );
            env.cpu.regs_mut()[0..2].fill(0);
            return;
        } else {
            panic!(
                "Item {class:?} in superclass chain of object {receiver:?}'s class {orig_class:?} has an unexpected host object type."
            );
        }
    }
}

/// Standard variant of `objc_msgSend`. See [objc_msgSend_inner].
#[allow(non_snake_case)]
pub(crate) fn objc_msgSend(env: &mut Environment, receiver: id, selector: SEL) {
    objc_msgSend_inner(
        env, receiver, selector, /* super2: */ None, /* tolerate_type_mismatch: */ false,
    )
}

#[allow(non_snake_case)]
pub(crate) fn _touchHLE_objc_msgSend_tolerant(env: &mut Environment, receiver: id, selector: SEL) {
    objc_msgSend_inner(
        env, receiver, selector, /* super2: */ None, /* tolerate_type_mismatch: */ true,
    )
}

/// Variant of `objc_msgSend` for methods that return a struct via a pointer.
/// See [objc_msgSend_inner].
///
/// The first parameter here is the pointer for the struct return. This is an
/// ABI detail that is usually hidden and handled behind-the-scenes by
/// [crate::abi], but `objc_msgSend` is a special case because of the
/// pass-through behaviour. Of course, the pass-through only works if the [IMP]
/// also has the pointer parameter. The caller therefore has to pick the
/// appropriate `objc_msgSend` variant depending on the method it wants to call.
pub(super) fn objc_msgSend_stret(
    env: &mut Environment,
    stret: MutVoidPtr,
    receiver: id,
    selector: SEL,
) {
    if zombie_farm_cell_content_size_override(env, receiver, selector, stret) {
        return;
    }
    objc_msgSend_inner(
        env, receiver, selector, /* super2: */ None, /* tolerate_type_mismatch: */ false,
    );
    trace_zombie_farm_layout_stret_return(env, receiver, selector, stret);
}

#[allow(non_snake_case)]
pub(crate) fn _touchHLE_objc_msgSend_stret_tolerant(
    env: &mut Environment,
    stret: MutVoidPtr,
    receiver: id,
    selector: SEL,
) {
    if zombie_farm_cell_content_size_override(env, receiver, selector, stret) {
        return;
    }
    objc_msgSend_inner(
        env, receiver, selector, /* super2: */ None, /* tolerate_type_mismatch: */ true,
    );
    trace_zombie_farm_layout_stret_return(env, receiver, selector, stret);
}

#[repr(C, packed)]
/// A pointer to this struct replaces the normal receiver parameter for
/// `objc_msgSendSuper2` and [msg_send_super2].
pub struct objc_super {
    pub receiver: id,
    /// If this is used with `objc_msgSendSuper` (not implemented here, TODO),
    /// this is a pointer to the superclass to look up the method on.
    /// If this is used with `objc_msgSendSuper2`, this is a pointer to a class
    /// and the superclass will be looked up from it.
    pub class: Class,
}
unsafe impl SafeRead for objc_super {}

/// Variant of `objc_msgSend` for supercalls. See [objc_msgSend_inner].
///
/// This variant has a weird ABI because it needs to receive an additional piece
/// of information (a class pointer), but it can't actually take this as an
/// extra parameter, because that would take one of the argument slots reserved
/// for arguments passed onto the method implementation. Hence the [objc_super]
/// pointer in place of the normal [id].
#[allow(non_snake_case)]
pub(super) fn objc_msgSendSuper2(
    env: &mut Environment,
    super_ptr: ConstPtr<objc_super>,
    selector: SEL,
) {
    let objc_super { receiver, class } = env.mem.read(super_ptr);

    // Rewrite first argument to match the normal ABI.
    crate::abi::write_next_arg(&mut 0, env.cpu.regs_mut(), &mut env.mem, receiver);

    objc_msgSend_inner(
        env,
        receiver,
        selector,
        /* super2: */ Some(class),
        /* tolerate_type_mismatch: */ false,
    )
}

/// Trait that assists with type-checking of [msg_send]'s arguments.
///
/// - Statically constrains the types of [msg_send]'s arguments so that the
///   first two are always [id] and [SEL].
/// - Provides the type ID to enable dynamic type checking of subsequent
///   arguments and the return type.
///
/// See `impl_HostIMP` for implementations. See also [MsgSendSuperSignature].
pub trait MsgSendSignature: 'static {
    /// Get the [TypeId] and a human-readable description for this signature.
    fn type_info() -> (TypeId, &'static str) {
        #[cfg(debug_assertions)]
        let type_name = std::any::type_name::<Self>();
        // Avoid wasting space on type names in release builds. At the time of
        // writing this saves about 36KB.
        #[cfg(not(debug_assertions))]
        let type_name = "[description unavailable in release builds]";
        (TypeId::of::<Self>(), type_name)
    }
}

/// Wrapper around [objc_msgSend] which, together with [msg], makes it easy to
/// send messages in host code. Warning: all types are inferred from the
/// call-site and they may not be checked, so be very sure you get them correct!
pub fn msg_send<R, P>(env: &mut Environment, args: P) -> R
where
    fn(&mut Environment, id, SEL): CallFromHost<R, P>,
    fn(&mut Environment, MutVoidPtr, id, SEL): CallFromHost<R, P>,
    (R, P): MsgSendSignature,
    R: GuestRet,
{
    // Provide type info for dynamic type checking.
    env.objc.message_type_info = Some(<(R, P) as MsgSendSignature>::type_info());
    if R::SIZE_IN_MEM.is_some() {
        (objc_msgSend_stret as fn(&mut Environment, MutVoidPtr, id, SEL)).call_from_host(env, args)
    } else {
        (objc_msgSend as fn(&mut Environment, id, SEL)).call_from_host(env, args)
    }
}

pub fn msg_send_no_type_checking<R, P>(env: &mut Environment, args: P) -> R
where
    fn(&mut Environment, id, SEL): CallFromHost<R, P>,
    fn(&mut Environment, MutVoidPtr, id, SEL): CallFromHost<R, P>,
    (R, P): MsgSendSignature,
    R: GuestRet,
{
    if R::SIZE_IN_MEM.is_some() {
        (_touchHLE_objc_msgSend_stret_tolerant as fn(&mut Environment, MutVoidPtr, id, SEL))
            .call_from_host(env, args)
    } else {
        (_touchHLE_objc_msgSend_tolerant as fn(&mut Environment, id, SEL)).call_from_host(env, args)
    }
}

/// Counterpart of [MsgSendSignature] for [msg_send_super2].
pub trait MsgSendSuperSignature: 'static {
    /// Signature with the [objc_super] pointer replaced by [id].
    type WithoutSuper: MsgSendSignature;
}

/// [msg_send] but for super-calls (calls [objc_msgSendSuper2]). You probably
/// want to use [msg_super] rather than calling this directly.
pub fn msg_send_super2<R, P>(env: &mut Environment, args: P) -> R
where
    fn(&mut Environment, ConstPtr<objc_super>, SEL): CallFromHost<R, P>,
    fn(&mut Environment, MutVoidPtr, ConstPtr<objc_super>, SEL): CallFromHost<R, P>,
    (R, P): MsgSendSuperSignature,
    R: GuestRet,
{
    // Provide type info for dynamic type checking.
    env.objc.message_type_info = Some(<(R, P) as MsgSendSuperSignature>::WithoutSuper::type_info());
    if R::SIZE_IN_MEM.is_some() {
        todo!() // no stret yet
    } else {
        (objc_msgSendSuper2 as fn(&mut Environment, ConstPtr<objc_super>, SEL))
            .call_from_host(env, args)
    }
}

/// Macro for sending a message which imitates the Objective-C messaging syntax.
/// See [msg_send] for the underlying implementation. Warning: all types are
/// inferred from the call-site and they may not be checked, so be very sure you
/// get them correct!
///
/// ```ignore
/// msg![env; foo setBar:bar withQux:qux];
/// ```
///
/// desugars to:
///
/// ```ignore
/// {
///     let sel = env.objc.lookup_selector("setFoo:withBar").unwrap();
///     msg_send(env, (foo, sel, bar, qux))
/// }
/// ```
///
/// Note that argument values that aren't a bare single identifier like `foo`
/// need to be bracketed.
///
/// See also [msg_class], if you want to send a message to a class.
#[macro_export]
macro_rules! msg {
    [$env:expr; $receiver:tt $name:ident $(: $arg1:tt $($($namen:ident)?: $argn:tt)*)?] => {
        {
            let sel = $crate::objc::selector!($($arg1;)? $name $($(, $($namen)?)*)?);
            let sel = $env.objc.lookup_selector(sel)
                .expect("Unknown selector");
            let args = ($receiver, sel, $($arg1, $($argn),*)?);
            $crate::objc::msg_send($env, args)
        }
    }
}
pub use crate::msg; // #[macro_export] is weird...

/// Variant of [msg] for super-calls.
///
/// Unlike the other variants, this macro can only be used within
/// [crate::objc::objc_classes], because it relies on that macro defining a
/// constant containing the name of the current class.
///
/// ```ignore
/// msg_super![env; this init]
/// ```
///
/// desugars to something like this, if the current class is `SomeClass`:
///
/// ```ignore
/// {
///     let super_arg_ptr = push_to_stack(env, objc_super {
///         receiver: this,
///         class: env.objc.get_known_class("SomeClass", &mut env.mem),
///     });
///     let sel = env.objc.lookup_selector("init").unwrap();
///     let res = msg_send_super2(env, (super_arg_ptr, sel));
///     pop_from_stack::<objc_super>(env);
///     res
/// }
/// ```
#[macro_export]
macro_rules! msg_super {
    [$env:expr; $receiver:tt $name:ident $(: $arg1:tt $($($namen:ident)?: $argn:tt)*)?] => {
        {
            let class = $env.objc.get_known_class(
                _OBJC_CURRENT_CLASS,
                &mut $env.mem
            );
            let sel = $crate::objc::selector!($($arg1;)? $name $($(, $($namen)?)*)?);
            let sel = $env.objc.lookup_selector(sel)
                .expect("Unknown selector");

            let sp = &mut $env.cpu.regs_mut()[$crate::cpu::Cpu::SP];
            let old_sp = *sp;
            *sp -= $crate::mem::guest_size_of::<$crate::objc::objc_super>();
            let super_ptr = $crate::mem::Ptr::from_bits(*sp);
            $env.mem.write(super_ptr, $crate::objc::objc_super {
                receiver: $receiver,
                class,
            });

            let args = (super_ptr.cast_const(), sel, $($arg1, $($argn),*)?);
            let res = $crate::objc::msg_send_super2($env, args);

            $env.cpu.regs_mut()[$crate::cpu::Cpu::SP] = old_sp;

            res
        }
    }
}
pub use crate::msg_super; // #[macro_export] is weird...

/// Variant of [msg] for sending a message to a named class. Useful for calling
/// class methods, especially `new`.
///
/// ```ignore
/// msg_class![env; SomeClass alloc]
/// ```
///
/// desugars to:
///
/// ```ignore
/// msg![env; (env.objc.get_known_class("SomeClass", &mut env.mem)) alloc]
/// ```
#[macro_export]
macro_rules! msg_class {
    [$env:expr; $receiver_class:ident $name:ident $(: $arg1:tt $($($namen:ident)?: $argn:tt)*)?] => {
        {
            let class = $env.objc.get_known_class(
                stringify!($receiver_class),
                &mut $env.mem
            );
            $crate::objc::msg![$env; class $name $(: $arg1 $($($namen)?: $argn)*)?]
        }
    }
}
pub use crate::msg_class; // #[macro_export] is weird...

/// Shorthand for `let _: id = msg![env; object retain];`
pub fn retain(env: &mut Environment, object: id) -> id {
    if object == nil {
        // fast path
        return nil;
    }
    msg![env; object retain]
}

/// Shorthand for `() = msg![env; object release];`
pub fn release(env: &mut Environment, object: id) {
    if object == nil {
        // fast path
        return;
    }
    msg![env; object release]
}

/// Shorthand for `let _: id = msg![env; object autorelease];`
pub fn autorelease(env: &mut Environment, object: id) -> id {
    if object == nil {
        // fast path
        return nil;
    }
    msg![env; object autorelease]
}
