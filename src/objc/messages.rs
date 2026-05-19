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
use crate::cpu::Cpu;
use crate::environment::ThreadId;
use crate::frameworks::core_graphics::{CGPoint, CGSize};
use crate::frameworks::foundation::{
    ns_date, ns_property_list_serialization, ns_string, NSUInteger,
};
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
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Mutex, OnceLock,
};

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
    let interesting_class = matches!(
        class_name,
        "ZFGuiLayer" | "ZFActorManager" | "GameState" | "GameData"
    );
    let interesting_selector = matches!(
        selector_name,
        "checkDailyEvent"
            | "getServerTime"
            | "handleTimeResponse:"
            | "handleResponse:forAction:"
            | "applyZombieHunger"
            | "statusCheckDone"
            | "startUpChecksComplete"
            | "fixZombieHunger"
            | "makeAllZombiesHungry"
            | "makeAllZombiesFull"
            | "saveGame"
            | "setSaveDate:"
            | "saveDate"
            | "getBeginningOfTheDayFromDate:"
    );
    interesting_class && interesting_selector
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

fn zombie_farm_sprite_trace_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("TOUCHHLE_ZF_SPRITE_TRACE").ok().as_deref() == Some("1"))
}

fn zombie_farm_touch_trace_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("TOUCHHLE_ZF2_TOUCH_TRACE").ok().as_deref() == Some("1"))
}

fn trace_zombie_farm_sprite_message(env: &Environment, receiver: id, selector_name: &str) {
    if !zombie_farm_sprite_trace_enabled() || !zombie_farm_uses_playforge_bundle(env) {
        return;
    }

    let Some(class_name) = zombie_farm_object_class_name(env, receiver) else {
        return;
    };
    let interesting_class = class_name.starts_with("SpecialStageActor")
        || class_name == "StageActor"
        || class_name == "ActorAttachment"
        || class_name == "CCSprite"
        || class_name == "CCSpriteSheet";
    if !interesting_class {
        return;
    }

    let details = match selector_name {
        "setPosition:" | "setAnchorPoint:" => {
            format!(" {}", point_arg_from_regs(env.cpu.regs(), 2))
        }
        "setScale:" | "setScaleX:" | "setScaleY:" | "setRotation:" => {
            format!(" {:.3}", f32::from_bits(env.cpu.regs()[2]))
        }
        "setVisible:" => format!(" {}", env.cpu.regs()[2] != 0),
        "setTextureRect:" => format!(" origin {}", point_arg_from_regs(env.cpu.regs(), 2)),
        _ => {
            if !matches!(
                selector_name,
                "setTextureRect:rotated:untrimmedSize:" | "setDisplayFrame:" | "setTexture:"
            ) {
                return;
            }
            String::new()
        }
    };

    log!(
        "ZombieFarm sprite trace: [{} {}] receiver {:?}{}",
        class_name,
        selector_name,
        receiver,
        details
    );
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

fn zombie_farm_status_arg_details(selector_name: &str, regs: &[u32]) -> Option<String> {
    match selector_name {
        "setHunger:" => Some(format!("arg hunger={:.3}", f32::from_bits(regs[2]))),
        "setEatDate:" | "setSaveDate:" | "handleTimeResponse:" => {
            Some(format!("arg object={:?}", id::from_bits(regs[2])))
        }
        "timeIntervalSinceDate:" | "getBeginningOfTheDayFromDate:" => {
            Some(format!("arg date={:?}", id::from_bits(regs[2])))
        }
        "addTimeInterval:" => {
            let mut bytes = [0u8; 8];
            bytes[0..4].copy_from_slice(&regs[2].to_le_bytes());
            bytes[4..8].copy_from_slice(&regs[3].to_le_bytes());
            Some(format!(
                "arg seconds={:.3}",
                f64::from_bits(u64::from_le_bytes(bytes))
            ))
        }
        "handleResponse:forAction:" => Some(format!(
            "arg response={:?} action={:?}",
            id::from_bits(regs[2]),
            id::from_bits(regs[3])
        )),
        _ => None,
    }
}

fn zombie_farm_uses_playforge_bundle(env: &Environment) -> bool {
    env.bundle
        .bundle_identifier()
        .starts_with("com.playforge.Z")
}

fn zombie_farm_object_class_name<'a>(env: &'a Environment, object: id) -> Option<&'a str> {
    if !zombie_farm_object_pointer_looks_valid(env, object) {
        return None;
    }
    let class = ObjC::read_isa(object, &env.mem);
    if class == nil {
        return None;
    }
    env.objc.try_get_class_name(class)
}

fn zombie_farm_object_pointer_looks_valid(env: &Environment, object: id) -> bool {
    object != nil && object.to_bits() >= env.mem.null_segment_size() && object.to_bits() % 4 == 0
}

fn zombie_farm_set_gui_layer_server_date_to_now(env: &mut Environment, receiver: id) -> bool {
    if !zombie_farm_uses_playforge_bundle(env)
        || zombie_farm_object_class_name(env, receiver) != Some("ZFGuiLayer")
    {
        return false;
    }

    let ivar_name = "serverDate".to_string();
    let Some(server_date_ivar) = env.objc.object_lookup_ivar(&env.mem, receiver, &ivar_name) else {
        return false;
    };

    let old_server_date: id = env.mem.read(server_date_ivar.cast());
    let now: id = msg_class![env; NSDate date];
    let now = retain(env, now);
    env.mem.write(server_date_ivar.cast(), now);
    if old_server_date != nil && old_server_date != now {
        release(env, old_server_date);
    }
    log!(
        "ZombieFarm status: using local NSDate {:?} as ZFGuiLayer.serverDate",
        now
    );
    true
}

static ZOMBIE_FARM_APPLIED_LOCAL_HUNGER: AtomicBool = AtomicBool::new(false);
static ZOMBIE_FARM_APPLY_TRACE_DEPTH: AtomicUsize = AtomicUsize::new(0);
static ZOMBIE_FARM_LAST_MAIN_MENU: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Default)]
struct ZombieFarmCocosTouchHandlers {
    targeted: Vec<u32>,
    standard: Vec<u32>,
    claimed_targeted: HashMap<u32, Vec<u32>>,
}

static ZOMBIE_FARM_COCOS_TOUCH_HANDLERS: OnceLock<
    Mutex<HashMap<u32, ZombieFarmCocosTouchHandlers>>,
> = OnceLock::new();

fn zombie_farm_cocos_touch_handlers() -> &'static Mutex<HashMap<u32, ZombieFarmCocosTouchHandlers>>
{
    ZOMBIE_FARM_COCOS_TOUCH_HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Clone, Copy, Default)]
struct ZombieFarmSyncOperationInfo {
    manager: u32,
    delegate: u32,
}

static ZOMBIE_FARM_SYNC_OPERATIONS: OnceLock<Mutex<HashMap<u32, ZombieFarmSyncOperationInfo>>> =
    OnceLock::new();

fn zombie_farm_sync_operations() -> &'static Mutex<HashMap<u32, ZombieFarmSyncOperationInfo>> {
    ZOMBIE_FARM_SYNC_OPERATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn zombie_farm_send_noarg_if_responds(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if receiver == nil {
        return false;
    }
    let Some(selector) = env.objc.lookup_selector(selector_name) else {
        return false;
    };
    if !env.objc.object_has_method(&env.mem, receiver, selector) {
        return false;
    }

    let regs = *env.cpu.regs();
    log!(
        "ZombieFarm2 workaround: sending [{} {}] during skipped startup sync",
        zombie_farm_object_class_name(env, receiver).unwrap_or("unknown"),
        selector_name
    );
    let _: () = msg_send_no_type_checking(env, (receiver, selector));
    env.cpu.regs_mut().copy_from_slice(&regs);
    true
}

fn zombie_farm_forward_backing_array_fast_enumeration(
    env: &mut Environment,
    receiver: id,
    selector: SEL,
    selector_name: &str,
) -> bool {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2"
        || selector_name != "countByEnumeratingWithState:objects:count:"
    {
        return false;
    }
    let Some(array_selector) = env.objc.lookup_selector("_array") else {
        return false;
    };
    if !env
        .objc
        .object_has_method(&env.mem, receiver, array_selector)
    {
        return false;
    }

    let regs = *env.cpu.regs();
    let backing_array: id = msg_send_no_type_checking(env, (receiver, array_selector));
    env.cpu.regs_mut().copy_from_slice(&regs);
    if backing_array == nil {
        return false;
    }

    let state = MutVoidPtr::from_bits(regs[2]);
    let objects = MutVoidPtr::from_bits(regs[3]);
    let count: NSUInteger = env.mem.read(ConstPtr::<u32>::from_bits(regs[Cpu::SP]));
    let result: NSUInteger =
        msg_send_no_type_checking(env, (backing_array, selector, state, objects, count));
    env.cpu.regs_mut().copy_from_slice(&regs);
    env.cpu.regs_mut()[0] = result;
    log_dbg!(
        "ZombieFarm2 workaround: forwarded fast enumeration for {} through backing array {:?}",
        zombie_farm_object_class_name(env, receiver).unwrap_or("unknown"),
        backing_array
    );
    true
}

fn zombie_farm_ignore_null_attachment_placeholder(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2"
        || zombie_farm_object_class_name(env, receiver) != Some("NSNull")
        || !matches!(
            selector_name,
            "calculateRotateValue"
                | "calculateBonePosition"
                | "rotateBone:aroundPoint:"
                | "flipBonePosition"
                | "rotateBone:aroundPoint:rotateChildren:"
                | "addBone:"
                | "setRotation:"
                | "setSprite:"
                | "setOriginalAnchor:"
                | "setOffsetFromRefPoint:"
                | "setLastFramePosition:"
                | "setAtlasRect:"
                | "setLastFrameRotation:"
                | "setChangeInRotation:"
                | "setLastFrameScale:"
                | "setInheritColor:"
                | "setCanSwap:"
                | "setChildAttachments:"
                | "setParentAttachment:"
                | "setOriginalRotation:"
                | "setFollowRotation:"
                | "setImage:"
                | "setAttachmentZOrder:"
                | "setFlippedAsset:"
                | "childAttachments"
                | "parentAttachment"
                | "sprite"
                | "image"
                | "originalAnchor"
                | "offsetFromRefPoint"
                | "lastFramePosition"
                | "atlasRect"
                | "lastFrameRotation"
                | "changeInRotation"
                | "lastFrameScale"
                | "inheritColor"
                | "canSwap"
                | "originalRotation"
                | "followRotation"
                | "attachmentZOrder"
                | "flippedAsset"
                | "tag"
                | "setTag:"
                | "parent"
                | "children"
                | "isVisible"
                | "visible"
                | "setVisible:"
                | "opacity"
                | "setOpacity:"
                | "rotation"
                | "scale"
                | "scaleX"
                | "scaleY"
                | "setScale:"
                | "setScaleX:"
                | "setScaleY:"
                | "addChild:"
                | "addChild:z:"
                | "removeFromParent"
                | "removeFromParentAndCleanup:"
                | "stopAllActions"
                | "runAction:"
                | "cleanup"
        )
    {
        return false;
    }

    env.cpu.regs_mut()[0] = 0;
    env.cpu.regs_mut()[1] = 0;
    log_dbg!(
        "ZombieFarm2 workaround: ignoring NSNull attachment placeholder [{}]",
        selector_name
    );
    true
}

fn zombie_farm_last_main_menu() -> Option<id> {
    let bits = ZOMBIE_FARM_LAST_MAIN_MENU.load(Ordering::Relaxed) as u32;
    (bits != 0).then_some(id::from_bits(bits))
}

fn zombie_farm_remove_view_controller_view(
    env: &mut Environment,
    controller: id,
    context: &str,
) -> bool {
    if controller == nil {
        return false;
    }
    let Some(view_selector) = env.objc.lookup_selector("view") else {
        return false;
    };
    if !env
        .objc
        .object_has_method(&env.mem, controller, view_selector)
    {
        return false;
    }

    let regs = *env.cpu.regs();
    let view: id = msg_send_no_type_checking(env, (controller, view_selector));
    env.cpu.regs_mut().copy_from_slice(&regs);
    if view == nil {
        return false;
    }

    let Some(remove_selector) = env.objc.lookup_selector("removeFromSuperview") else {
        return false;
    };
    if !env.objc.object_has_method(&env.mem, view, remove_selector) {
        return false;
    }

    let regs = *env.cpu.regs();
    let _: () = msg_send_no_type_checking(env, (view, remove_selector));
    env.cpu.regs_mut().copy_from_slice(&regs);
    log!(
        "ZombieFarm2 workaround: removed {} view {:?} from superview",
        context,
        view
    );
    true
}

fn zombie_farm_remove_stale_uikit_subviews_by_class(env: &mut Environment, class_name: &str) {
    let removed = crate::frameworks::uikit::ui_view::remove_subviews_by_class(env, class_name);
    if removed > 0 {
        log!(
            "ZombieFarm2 workaround: removed {} stale {} UIKit view(s)",
            removed,
            class_name
        );
    }
}

fn zombie_farm_reveal_hud_controls(env: &mut Environment, context: &str) {
    let revealed = crate::frameworks::uikit::ui_view::reveal_zombie_farm_hud_controls(env);
    if revealed > 0 {
        log!(
            "ZombieFarm2 workaround: revealed {} hidden HUD/UIKit control(s) after {}",
            revealed,
            context
        );
    }
}

fn zombie_farm_get_gui_layer(env: &mut Environment) -> Option<id> {
    let gui_layer_class = env.objc.get_known_class("ZFGuiLayer", &mut env.mem);
    let gui_selector = env.objc.lookup_selector("gui")?;
    if !env
        .objc
        .object_has_method(&env.mem, gui_layer_class, gui_selector)
    {
        return None;
    }
    let gui_layer: id = msg_send_no_type_checking(env, (gui_layer_class, gui_selector));
    (gui_layer != nil).then_some(gui_layer)
}

fn zombie_farm_get_game_state(env: &mut Environment) -> Option<id> {
    let game_state_class = env.objc.get_known_class("GameState", &mut env.mem);
    let game_state_selector = env.objc.lookup_selector("gameState")?;
    if !env
        .objc
        .object_has_method(&env.mem, game_state_class, game_state_selector)
    {
        return None;
    }
    let game_state: id = msg_send_no_type_checking(env, (game_state_class, game_state_selector));
    (game_state != nil).then_some(game_state)
}

fn zombie_farm_get_running_scene(env: &mut Environment) -> Option<id> {
    let director_class = env.objc.get_known_class("CCDirector", &mut env.mem);
    let shared_director_selector = env.objc.lookup_selector("sharedDirector")?;
    if !env
        .objc
        .object_has_method(&env.mem, director_class, shared_director_selector)
    {
        return None;
    }
    let director: id = msg_send_no_type_checking(env, (director_class, shared_director_selector));
    if director == nil {
        return None;
    }
    let running_scene_selector = env.objc.lookup_selector("runningScene")?;
    if !env
        .objc
        .object_has_method(&env.mem, director, running_scene_selector)
    {
        return None;
    }
    let running_scene: id = msg_send_no_type_checking(env, (director, running_scene_selector));
    (running_scene != nil).then_some(running_scene)
}

fn zombie_farm_find_cocos_child_by_class(
    env: &mut Environment,
    root: id,
    class_name: &str,
    depth: usize,
) -> Option<id> {
    if root == nil || depth > 8 {
        return None;
    }
    if zombie_farm_object_class_name(env, root) == Some(class_name) {
        return Some(root);
    }

    let children_selector = env.objc.lookup_selector("children")?;
    if !env
        .objc
        .object_has_method(&env.mem, root, children_selector)
    {
        return None;
    }
    let children: id = msg_send_no_type_checking(env, (root, children_selector));
    if children == nil {
        return None;
    }

    let count_selector = env.objc.lookup_selector("count")?;
    let object_at_index_selector = env.objc.lookup_selector("objectAtIndex:")?;
    if !env
        .objc
        .object_has_method(&env.mem, children, count_selector)
        || !env
            .objc
            .object_has_method(&env.mem, children, object_at_index_selector)
    {
        return None;
    }
    let count: NSUInteger = msg_send_no_type_checking(env, (children, count_selector));
    for idx in 0..count.min(200) {
        let child: id = msg_send_no_type_checking(env, (children, object_at_index_selector, idx));
        if let Some(found) =
            zombie_farm_find_cocos_child_by_class(env, child, class_name, depth + 1)
        {
            return Some(found);
        }
    }
    None
}

fn zombie_farm_get_farm_tile_map(env: &mut Environment) -> Option<id> {
    let running_scene = zombie_farm_get_running_scene(env)?;
    if let Some(farm_tile_map_selector) = env.objc.lookup_selector("farmTileMap") {
        if env
            .objc
            .object_has_method(&env.mem, running_scene, farm_tile_map_selector)
        {
            let tile_map: id =
                msg_send_no_type_checking(env, (running_scene, farm_tile_map_selector));
            if tile_map != nil {
                return Some(tile_map);
            }
        }
    }
    zombie_farm_find_cocos_child_by_class(env, running_scene, "ZFFarmTileMap", 0)
}

fn zombie_farm_get_actor_list(env: &mut Environment) -> Option<id> {
    let game_state = zombie_farm_get_game_state(env)?;
    let zf_game_data_selector = env.objc.lookup_selector("zfGameData")?;
    if !env
        .objc
        .object_has_method(&env.mem, game_state, zf_game_data_selector)
    {
        return None;
    }
    let game_data: id = msg_send_no_type_checking(env, (game_state, zf_game_data_selector));
    if game_data == nil {
        return None;
    }

    let actor_list_selector = env.objc.lookup_selector("actorList")?;
    if !env
        .objc
        .object_has_method(&env.mem, game_data, actor_list_selector)
    {
        return None;
    }
    let actor_list: id = msg_send_no_type_checking(env, (game_data, actor_list_selector));
    (actor_list != nil).then_some(actor_list)
}

fn zombie_farm_get_game_data(env: &mut Environment) -> Option<id> {
    let game_state = zombie_farm_get_game_state(env)?;
    let zf_game_data_selector = env.objc.lookup_selector("zfGameData")?;
    if !env
        .objc
        .object_has_method(&env.mem, game_state, zf_game_data_selector)
    {
        return None;
    }
    let game_data: id = msg_send_no_type_checking(env, (game_state, zf_game_data_selector));
    (game_data != nil).then_some(game_data)
}

fn zombie_farm_get_live_actor_list(env: &mut Environment) -> Option<id> {
    let actor_manager_class = env.objc.get_known_class("ZFActorManager", &mut env.mem);
    let actor_manager_selector = env.objc.lookup_selector("actorManager")?;
    if !env
        .objc
        .object_has_method(&env.mem, actor_manager_class, actor_manager_selector)
    {
        return None;
    }
    let actor_manager: id =
        msg_send_no_type_checking(env, (actor_manager_class, actor_manager_selector));
    if actor_manager == nil {
        return None;
    }

    let actor_list_selector = env.objc.lookup_selector("actorList")?;
    if !env
        .objc
        .object_has_method(&env.mem, actor_manager, actor_list_selector)
    {
        return None;
    }
    let actor_list: id = msg_send_no_type_checking(env, (actor_manager, actor_list_selector));
    (actor_list != nil).then_some(actor_list)
}

fn zombie_farm_actor_is_zombie(env: &Environment, actor: id) -> bool {
    zombie_farm_object_class_name(env, actor)
        .is_some_and(|class_name| class_name.starts_with("ZombieActor"))
}

fn zombie_farm_write_actor_hunger_ivar(env: &mut Environment, actor: id, hunger: f32) -> bool {
    let ivar_name = "hunger".to_string();
    let Some(ivar) = env.objc.object_lookup_ivar(&env.mem, actor, &ivar_name) else {
        return false;
    };
    env.mem.write(ivar.cast(), hunger);
    true
}

fn zombie_farm_read_actor_hunger_ivar(env: &Environment, actor: id) -> Option<f32> {
    let ivar_name = "hunger".to_string();
    let ivar = env.objc.object_lookup_ivar(&env.mem, actor, &ivar_name)?;
    Some(env.mem.read(ivar.cast()))
}

fn zombie_farm_read_object_ivar(env: &Environment, object: id, ivar_name: &str) -> Option<id> {
    if !zombie_farm_object_pointer_looks_valid(env, object) {
        return None;
    }
    let ivar_name = ivar_name.to_string();
    let ivar = env.objc.object_lookup_ivar(&env.mem, object, &ivar_name)?;
    Some(env.mem.read(ivar.cast()))
}

fn zombie_farm_date_interval(env: &Environment, date: id) -> Option<f64> {
    if !zombie_farm_object_pointer_looks_valid(env, date) {
        return None;
    }
    ns_date::debug_time_interval(env, date)
}

fn zombie_farm_actor_eat_date(env: &mut Environment, actor: id) -> Option<id> {
    let Some(eat_date_selector) = env.objc.lookup_selector("eatDate") else {
        return zombie_farm_read_object_ivar(env, actor, "eatDate")
            .filter(|date| zombie_farm_date_interval(env, *date).is_some());
    };
    if !env
        .objc
        .object_has_method(&env.mem, actor, eat_date_selector)
    {
        return zombie_farm_read_object_ivar(env, actor, "eatDate")
            .filter(|date| zombie_farm_date_interval(env, *date).is_some());
    }

    let eat_date: id = msg_send_no_type_checking(env, (actor, eat_date_selector));
    if zombie_farm_date_interval(env, eat_date).is_some() {
        Some(eat_date)
    } else {
        zombie_farm_read_object_ivar(env, actor, "eatDate")
            .filter(|date| zombie_farm_date_interval(env, *date).is_some())
    }
}

fn zombie_farm_oldest_zombie_eat_date_in_list(
    env: &mut Environment,
    actor_list: id,
) -> Option<(id, f64)> {
    let count_selector = env.objc.lookup_selector("count")?;
    let object_at_index_selector = env.objc.lookup_selector("objectAtIndex:")?;
    if !env
        .objc
        .object_has_method(&env.mem, actor_list, count_selector)
        || !env
            .objc
            .object_has_method(&env.mem, actor_list, object_at_index_selector)
    {
        return None;
    }

    let count: NSUInteger = msg_send_no_type_checking(env, (actor_list, count_selector));
    let mut oldest: Option<(id, f64)> = None;
    for idx in 0..count {
        let actor: id = msg_send_no_type_checking(env, (actor_list, object_at_index_selector, idx));
        if actor == nil || !zombie_farm_actor_is_zombie(env, actor) {
            continue;
        }

        let Some(eat_date) = zombie_farm_actor_eat_date(env, actor) else {
            continue;
        };
        let Some(interval) = zombie_farm_date_interval(env, eat_date) else {
            continue;
        };

        if oldest
            .as_ref()
            .is_none_or(|(_, oldest_interval)| interval < *oldest_interval)
        {
            oldest = Some((eat_date, interval));
        }
    }

    oldest
}

fn zombie_farm_oldest_zombie_eat_date(env: &mut Environment) -> Option<(id, f64)> {
    let mut oldest: Option<(id, f64)> = None;
    for actor_list in [
        zombie_farm_get_actor_list(env),
        zombie_farm_get_live_actor_list(env),
    ]
    .into_iter()
    .flatten()
    {
        let Some((eat_date, interval)) =
            zombie_farm_oldest_zombie_eat_date_in_list(env, actor_list)
        else {
            continue;
        };
        if oldest
            .as_ref()
            .is_none_or(|(_, oldest_interval)| interval < *oldest_interval)
        {
            oldest = Some((eat_date, interval));
        }
    }
    oldest
}

fn zombie_farm_game_state_save_date(env: &mut Environment, game_state: id) -> Option<id> {
    if let Some(save_date) = zombie_farm_read_object_ivar(env, game_state, "saveDate")
        .filter(|date| zombie_farm_date_interval(env, *date).is_some())
    {
        return Some(save_date);
    }

    let save_date_selector = env.objc.lookup_selector("saveDate")?;
    if !env
        .objc
        .object_has_method(&env.mem, game_state, save_date_selector)
    {
        return None;
    }
    let save_date: id = msg_send_no_type_checking(env, (game_state, save_date_selector));
    (zombie_farm_date_interval(env, save_date).is_some()).then_some(save_date)
}

fn zombie_farm_set_game_state_save_date(env: &mut Environment, game_state: id, save_date: id) {
    if !zombie_farm_object_pointer_looks_valid(env, save_date) {
        return;
    }

    let regs = *env.cpu.regs();
    let Some(save_date_ivar) =
        env.objc
            .object_lookup_ivar(&env.mem, game_state, &"saveDate".to_string())
    else {
        return;
    };
    let old_save_date: id = env.mem.read(save_date_ivar.cast());
    let retained_save_date = retain(env, save_date);
    env.mem.write(save_date_ivar.cast(), retained_save_date);
    if old_save_date != nil && old_save_date != retained_save_date {
        release(env, old_save_date);
    }
    env.cpu.regs_mut().copy_from_slice(&regs);
}

fn zombie_farm_ensure_game_state_save_date(env: &mut Environment) -> bool {
    if !zombie_farm_uses_playforge_bundle(env) {
        return false;
    }

    let Some(game_state) = zombie_farm_get_game_state(env) else {
        return false;
    };
    if zombie_farm_game_state_save_date(env, game_state).is_some() {
        return true;
    }

    let Some((save_date, interval)) = zombie_farm_oldest_zombie_eat_date(env) else {
        return false;
    };
    zombie_farm_set_game_state_save_date(env, game_state, save_date);
    log!(
        "ZombieFarm status: restored nil GameState.saveDate from oldest zombie eatDate {:?} ({:.3}s since Apple epoch)",
        save_date,
        interval
    );
    true
}

fn zombie_farm_prepare_game_state_save_date(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) {
    if !zombie_farm_uses_playforge_bundle(env)
        || selector_name != "setSaveDate:"
        || zombie_farm_object_class_name(env, receiver) != Some("GameState")
        || id::from_bits(env.cpu.regs()[2]) != nil
    {
        return;
    }

    let regs = *env.cpu.regs();

    if let Some(existing_save_date) = zombie_farm_game_state_save_date(env, receiver) {
        env.cpu.regs_mut().copy_from_slice(&regs);
        log_dbg!(
            "ZombieFarm status: preserving existing GameState.saveDate {:?} for setSaveDate:nil",
            existing_save_date
        );
        return;
    }

    let save_date = if let Some((oldest_eat_date, interval)) =
        zombie_farm_oldest_zombie_eat_date(env)
    {
        log!(
            "ZombieFarm status: replacing GameState setSaveDate:nil with oldest zombie eatDate {:?} ({:.3}s since Apple epoch)",
            oldest_eat_date,
            interval
        );
        oldest_eat_date
    } else {
        let now: id = msg_class![env; NSDate date];
        log!(
            "ZombieFarm status: replacing GameState setSaveDate:nil with local NSDate {:?}",
            now
        );
        now
    };
    env.cpu.regs_mut().copy_from_slice(&regs);
    env.cpu.regs_mut()[2] = save_date.to_bits();
}

fn zombie_farm_skip_epic_event_with_missing_remote_data(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2"
        || selector_name != "validBossID:"
        || zombie_farm_object_class_name(env, receiver) != Some("EpicEventManager")
    {
        return false;
    }

    env.cpu.regs_mut()[0] = 0;
    log!(
        "ZombieFarm2 workaround: treating EpicEventManager validBossID:{} as false",
        env.cpu.regs()[2]
    );
    true
}

fn zombie_farm_override_cocos2d_get_zeye(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2" || selector_name != "getZEye" {
        return false;
    }

    let class = ObjC::read_isa(receiver, &env.mem);
    let Some(class_name) = env.objc.try_get_class_name(class) else {
        return false;
    };
    if !matches!(
        class_name,
        "CCDirector" | "CCFastDirector" | "CCThreadedFastDirector"
    ) {
        return false;
    }

    let (_, height) = env.window().device_family().portrait_size();
    let z_eye = height as f32 / 1.1566;
    env.cpu.regs_mut()[0] = z_eye.to_bits();
    log!(
        "ZombieFarm2 workaround: [{} getZEye] -> {:.3}",
        class_name,
        z_eye
    );
    true
}

fn zombie_farm_skip_cocos2d_projection_setup(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2"
        || selector_name != "setProjection:"
        || std::env::var("TOUCHHLE_ZF2_SKIP_PROJECTION")
            .ok()
            .as_deref()
            != Some("1")
    {
        return false;
    }

    let class = ObjC::read_isa(receiver, &env.mem);
    let Some(class_name) = env.objc.try_get_class_name(class) else {
        return false;
    };
    if !matches!(
        class_name,
        "CCDirector" | "CCFastDirector" | "CCThreadedFastDirector"
    ) {
        return false;
    }

    log!(
        "ZombieFarm2 workaround: ignoring [{} setProjection:{}]",
        class_name,
        env.cpu.regs()[2]
    );
    true
}

fn zombie_farm_skip_remote_asset_requests(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2"
        || !matches!(
            selector_name,
            "startRemote"
                | "requestManifest"
                | "downloadRemote"
                | "requestAssets"
                | "rerunFailedRequests"
        )
        || zombie_farm_object_class_name(env, receiver) != Some("RemoteManager")
    {
        return false;
    }

    log!(
        "ZombieFarm2 workaround: skipping RemoteManager {}",
        selector_name
    );
    true
}

fn zombie_farm_skip_event_tracker_nil_last_event(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2"
        || selector_name != "setLastEvent:"
        || env.cpu.regs()[2] != nil.to_bits()
    {
        return false;
    }

    let Some(class_name) = zombie_farm_object_class_name(env, receiver) else {
        return false;
    };
    if !matches!(class_name, "EventTracker" | "ZF2EventTracker") {
        return false;
    }

    log!(
        "ZombieFarm2 workaround: ignoring [{} setLastEvent:nil]",
        class_name
    );
    true
}

fn zombie_farm_skip_event_tracker_init(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2" || selector_name != "init" {
        return false;
    }

    let Some(class_name) = zombie_farm_object_class_name(env, receiver).map(str::to_string) else {
        return false;
    };
    if !matches!(class_name.as_str(), "EventTracker" | "ZF2EventTracker") {
        return false;
    }

    env.cpu.regs_mut()[0] = receiver.to_bits();
    log!("ZombieFarm2 workaround: host-handled [{} init]", class_name);
    true
}

fn zombie_farm_return_open_udid(env: &mut Environment, receiver: id, selector_name: &str) -> bool {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2"
        || !matches!(
            selector_name,
            "value" | "valueWithError:" | "_getOpenUDID" | "_generateFreshOpenUDID"
        )
    {
        return false;
    }

    let Some(class_name) = zombie_farm_object_class_name(env, receiver).map(str::to_string) else {
        return false;
    };
    if !matches!(
        class_name.as_str(),
        "VungleOpenUDID" | "OpenUDID" | "AP_OpenUDID"
    ) {
        return false;
    }

    let udid =
        ns_string::from_rust_string(env, "0000000000000000000000000000000000000000".to_string());
    env.cpu.regs_mut()[0] = udid.to_bits();
    log!(
        "ZombieFarm2 workaround: [{} {}] -> fixed OpenUDID",
        class_name,
        selector_name
    );
    true
}

fn zombie_farm_skip_vungle_ad_sdk(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2" {
        return false;
    }

    let Some(class_name) = zombie_farm_object_class_name(env, receiver).map(str::to_string) else {
        return false;
    };

    let should_skip = if class_name == "VungleSDK" {
        matches!(
            selector_name,
            "startWithAppId:"
                | "startWithAppId:delegate:"
                | "loadDatabase"
                | "updateUserAgent"
                | "setSafariUserAgent:"
                | "setDelegate:"
                | "setLoggingEnabled:"
                | "setUserData:"
                | "setIncentivizedDelegate:"
                | "cacheAd"
                | "playAd"
                | "getPreferenceValueForKey:"
                | "setPreferenceValue:forKey:"
        )
    } else if class_name == "VungleCacheManager" {
        matches!(selector_name, "createDirectory:")
    } else if class_name.starts_with("VungleFMDatabaseQueue") {
        matches!(
            selector_name,
            "inDatabase:"
                | "inTransaction:"
                | "inDeferredTransaction:"
                | "database"
                | "close"
                | "checkpoint:error:"
        )
    } else if class_name.starts_with("VungleFMDatabase") {
        matches!(
            selector_name,
            "open"
                | "openWithFlags:"
                | "close"
                | "executeQuery:"
                | "executeQuery:withArgumentsInArray:orDictionary:orVAList:"
                | "executeUpdate:"
                | "executeUpdate:withArgumentsInArray:orDictionary:orVAList:"
                | "executeStatements:"
                | "lastErrorCode"
                | "lastErrorMessage"
        )
    } else {
        false
    };

    if !should_skip {
        return false;
    }

    env.cpu.regs_mut()[0] = 0;
    log!(
        "ZombieFarm2 workaround: skipping Vungle ad SDK [{} {}]",
        class_name,
        selector_name
    );
    true
}

fn zombie_farm_skip_broken_font_preload(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2"
        || !matches!(
            selector_name,
            "loadFonts" | "loadFont:" | "loadFont:withName:"
        )
        || zombie_farm_object_class_name(env, receiver) != Some("ZombieFarmAppDelegate")
    {
        return false;
    }

    env.cpu.regs_mut()[0] = 0;
    log!(
        "ZombieFarm2 workaround: skipping ZombieFarmAppDelegate {}",
        selector_name
    );
    true
}

fn zombie_farm_skip_brain_client_network(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2" {
        return false;
    }

    let Some(class_name) = zombie_farm_object_class_name(env, receiver).map(str::to_string) else {
        return false;
    };

    let should_skip = if class_name == "BrainClient" {
        selector_name.starts_with("sendRequestTo:")
            || matches!(
                selector_name,
                "cancelRequestsForDelegate:"
                    | "setAsynchronousOperations:"
                    | "setSynchronousOperations:"
            )
    } else if class_name == "BrainClientOperation" {
        matches!(
            selector_name,
            "start"
                | "main"
                | "createPostRequest"
                | "sendRequest"
                | "connection:didReceiveResponse:"
                | "connection:didReceiveData:"
                | "connectionDidFinishLoading:"
                | "connection:didFailWithError:"
        )
    } else if class_name == "NSOperationQueue" && selector_name == "addOperation:" {
        let operation = id::from_bits(env.cpu.regs()[2]);
        zombie_farm_object_class_name(env, operation) == Some("BrainClientOperation")
    } else {
        false
    };

    if !should_skip {
        return false;
    }

    env.cpu.regs_mut()[0] = 0;
    log!(
        "ZombieFarm2 workaround: skipping BrainClient network [{} {}]",
        class_name,
        selector_name
    );
    true
}

fn zombie_farm_skip_sync_queue_network(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2" {
        return false;
    }

    let Some(class_name) = zombie_farm_object_class_name(env, receiver).map(str::to_string) else {
        return false;
    };

    let is_sync_operation = matches!(
        class_name.as_str(),
        "SyncOperation"
            | "SetupOperation"
            | "SetupActiveOperation"
            | "LoadSaveOperation"
            | "SyncSaveOperation"
            | "DownloadSaveOperation"
            | "UploadSaveOperation"
            | "SaveGameOperation"
    );
    if is_sync_operation
        && matches!(
            selector_name,
            "initWithManager:andDelegate:" | "initWithManager:delegate:" | "initWithDelegate:"
        )
    {
        let (manager, delegate) = if selector_name == "initWithDelegate:" {
            (nil, id::from_bits(env.cpu.regs()[2]))
        } else {
            (
                id::from_bits(env.cpu.regs()[2]),
                id::from_bits(env.cpu.regs()[3]),
            )
        };
        zombie_farm_sync_operations().lock().unwrap().insert(
            receiver.to_bits(),
            ZombieFarmSyncOperationInfo {
                manager: manager.to_bits(),
                delegate: delegate.to_bits(),
            },
        );
        env.cpu.regs_mut()[0] = receiver.to_bits();
        log!(
            "ZombieFarm2 workaround: host-handled SyncQueue operation init [{} {}] manager {:?} delegate {:?} ({})",
            class_name,
            selector_name,
            manager,
            delegate,
            zombie_farm_object_class_name(env, delegate).unwrap_or("unknown")
        );
        return true;
    }

    let should_skip = if class_name == "SyncQueue" {
        matches!(
            selector_name,
            "addOperation:"
                | "addOperation:withPriority:"
                | "addOperations:waitUntilFinished:"
                | "cancelAllOperations"
        )
    } else if is_sync_operation {
        matches!(selector_name, "start" | "main" | "cancel")
    } else if class_name == "NSOperationQueue" && selector_name == "addOperation:" {
        let operation = id::from_bits(env.cpu.regs()[2]);
        zombie_farm_object_class_name(env, operation).is_some_and(|operation_class| {
            operation_class == "SyncOperation"
                || operation_class == "SetupOperation"
                || operation_class.ends_with("Operation") && operation_class.contains("Save")
                || operation_class == "SetupActiveOperation"
        })
    } else {
        false
    };

    if !should_skip {
        return false;
    }

    let skipped_setup_active_operation = matches!(
        selector_name,
        "addOperation:" | "addOperation:withPriority:"
    ) && {
        let operation = id::from_bits(env.cpu.regs()[2]);
        matches!(
            zombie_farm_object_class_name(env, operation),
            Some("SetupOperation" | "SetupActiveOperation")
        )
    };
    if skipped_setup_active_operation {
        let operation = id::from_bits(env.cpu.regs()[2]);
        zombie_farm_complete_skipped_setup_operation(env, operation);
    }

    env.cpu.regs_mut()[0] = 0;
    log!(
        "ZombieFarm2 workaround: skipping SyncQueue network [{} {}]",
        class_name,
        selector_name
    );
    true
}

fn zombie_farm_complete_skipped_setup_operation(env: &mut Environment, operation: id) {
    let info = zombie_farm_sync_operations()
        .lock()
        .unwrap()
        .get(&operation.to_bits())
        .copied();
    let Some(info) = info else {
        zombie_farm_finish_skipped_startup_sync(env);
        return;
    };

    let delegate = id::from_bits(info.delegate);
    let manager = id::from_bits(info.manager);
    log!(
        "ZombieFarm2 workaround: completing skipped {:?} ({}) manager {:?} delegate {:?} ({})",
        operation,
        zombie_farm_object_class_name(env, operation).unwrap_or("unknown"),
        manager,
        delegate,
        zombie_farm_object_class_name(env, delegate).unwrap_or("unknown")
    );

    if zombie_farm_object_class_name(env, delegate) == Some("LoadingScreen")
        && zombie_farm_send_noarg_if_responds(env, delegate, "loadFarmScene")
    {
        let _ = zombie_farm_remove_view_controller_view(env, delegate, "LoadingScreen");
        if let Some(main_menu) = zombie_farm_last_main_menu() {
            let _ = zombie_farm_remove_view_controller_view(env, main_menu, "MainMenu");
        }
        zombie_farm_remove_stale_uikit_subviews_by_class(env, "WhiteDimLayer");
        zombie_farm_reveal_hud_controls(env, "LoadingScreen loadFarmScene");
        zombie_farm_finish_skipped_startup_sync(env);
        return;
    }

    let sent_sync_finished = zombie_farm_send_noarg_if_responds(env, delegate, "syncFinished");
    if zombie_farm_get_farm_tile_map(env).is_none() {
        zombie_farm_send_noarg_if_responds(env, delegate, "startGame");
    }
    if !sent_sync_finished && zombie_farm_get_farm_tile_map(env).is_none() {
        zombie_farm_finish_skipped_startup_sync(env);
    }
}

fn zombie_farm_finish_skipped_startup_sync(env: &mut Environment) {
    let Some(gui_layer) = zombie_farm_get_gui_layer(env) else {
        return;
    };
    let Some(startup_complete_selector) = env.objc.lookup_selector("startUpChecksComplete") else {
        return;
    };
    if !env
        .objc
        .object_has_method(&env.mem, gui_layer, startup_complete_selector)
    {
        return;
    }

    let regs = *env.cpu.regs();
    log!("ZombieFarm2 workaround: completing skipped startup sync locally");
    let _: () = msg_send_no_type_checking(env, (gui_layer, startup_complete_selector));
    env.cpu.regs_mut().copy_from_slice(&regs);
    zombie_farm_reveal_hud_controls(env, "startup sync");
}

fn zombie_farm_return_safe_game_state_count(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2"
        || zombie_farm_object_class_name(env, receiver) != Some("GameState")
    {
        return false;
    }

    let value = match selector_name {
        "curArmySize" | "armySize" => 0,
        "maxArmySize" => 8,
        _ => return false,
    };

    env.cpu.regs_mut()[0] = value;
    log!(
        "ZombieFarm2 workaround: [{} {}] -> {}",
        "GameState",
        selector_name,
        value
    );
    true
}

fn zombie_farm_skip_remote_dependent_game_state_update(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2"
        || zombie_farm_object_class_name(env, receiver) != Some("GameState")
        || selector_name != "updateRemoteDataDependantSystems"
    {
        return false;
    }

    env.cpu.regs_mut()[0] = 0;
    log!("ZombieFarm2 workaround: skipping [GameState updateRemoteDataDependantSystems]");
    true
}

fn zombie_farm_return_self_for_game_data_copy(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2"
        || zombie_farm_object_class_name(env, receiver) != Some("GameData")
        || !matches!(
            selector_name,
            "copyWithZone:" | "mutableCopyWithZone:" | "copy"
        )
    {
        return false;
    }

    env.cpu.regs_mut()[0] = retain(env, receiver).to_bits();
    log!(
        "ZombieFarm2 workaround: returning retained self for [GameData {}]",
        selector_name
    );
    true
}

fn zombie_farm_host_actor_manager_init(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2"
        || zombie_farm_object_class_name(env, receiver) != Some("ZFActorManager")
    {
        return false;
    }

    if matches!(selector_name, "deleteAllFarmActors" | "removeAllActors") {
        env.cpu.regs_mut()[0] = 0;
        log!(
            "ZombieFarm2 workaround: skipping [ZFActorManager {}]",
            selector_name
        );
        return true;
    }

    false
}

fn zombie_farm_skip_tool_manager_transient_actions(
    _env: &mut Environment,
    _receiver: id,
    _selector_name: &str,
) -> bool {
    false
}

fn zombie_farm_skip_quest_manager_reset(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2"
        || zombie_farm_object_class_name(env, receiver) != Some("ZFQuestMan")
        || !matches!(selector_name, "reset" | "enable:")
    {
        return false;
    }

    env.cpu.regs_mut()[0] = 0;
    log!(
        "ZombieFarm2 workaround: skipping [ZFQuestMan {}]",
        selector_name
    );
    true
}

fn zombie_farm_skip_unsafe_toolbar_build(
    _env: &mut Environment,
    _receiver: id,
    _selector_name: &str,
) -> bool {
    false
}

fn zombie_farm_skip_market_offers(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2"
        || zombie_farm_object_class_name(env, receiver) != Some("MarketDataManager")
        || !matches!(selector_name, "offersAvailable" | "hasOffersAvailable")
    {
        return false;
    }

    env.cpu.regs_mut()[0] = 0;
    log!(
        "ZombieFarm2 workaround: treating [MarketDataManager {}] as false",
        selector_name
    );
    true
}

fn zombie_farm_skip_event_ad_networks(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2" {
        return false;
    }

    let Some(class_name) = zombie_farm_object_class_name(env, receiver).map(str::to_string) else {
        return false;
    };

    let should_skip = if matches!(class_name.as_str(), "EventTracker" | "ZF2EventTracker") {
        matches!(
            selector_name,
            "setupAdNetworks"
                | "setupFlurryAds"
                | "fetchFlurryAds"
                | "fetchFlurryAdForSpace:"
                | "isFlurryAdAvailableForSpace:"
                | "setupKiipAds"
                | "showFlurryAdForSpace:"
                | "showKiipAd:"
        )
    } else {
        class_name == "Kiip"
            || class_name.starts_with("Kiip")
            || class_name == "Flurry"
            || class_name.starts_with("Flurry")
            || class_name == "Chartboost"
            || class_name.starts_with("Chartboost")
            || class_name.starts_with("AdColony")
            || class_name.starts_with("ADC")
    };

    if !should_skip {
        return false;
    }

    env.cpu.regs_mut()[0] = if selector_name.starts_with("init") {
        receiver.to_bits()
    } else {
        0
    };
    log!(
        "ZombieFarm2 workaround: skipping ad network [{} {}]",
        class_name,
        selector_name
    );
    true
}

fn zombie_farm_skip_startup_profile_detection(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2"
        || zombie_farm_object_class_name(env, receiver) != Some("PlayerProfileManager")
        || !matches!(
            selector_name,
            "determineStartupPlayer"
                | "determineStartupPlayer:"
                | "updatePlayerInfo"
                | "showStartupPlayerSelection"
                | "showStartupPlayerSelection:"
        )
    {
        return false;
    }

    env.cpu.regs_mut()[0] = 0;
    log!(
        "ZombieFarm2 workaround: skipping PlayerProfileManager {}",
        selector_name
    );
    true
}

fn zombie_farm_skip_startup_internet_loading(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2" {
        return false;
    }

    let Some(class_name) = zombie_farm_object_class_name(env, receiver).map(str::to_string) else {
        return false;
    };
    let should_skip = (class_name == "MainMenu" && selector_name == "startupInternet")
        || (class_name == "LoadingScreen"
            && matches!(
                selector_name,
                "updateLoadingScreens" | "show" | "hide" | "show:" | "hide:"
            ));

    if !should_skip {
        return false;
    }

    env.cpu.regs_mut()[0] = 0;
    log!(
        "ZombieFarm2 workaround: skipping startup loading [{} {}]",
        class_name,
        selector_name
    );
    true
}

fn zombie_farm_host_load_farm_scene(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2"
        || zombie_farm_object_class_name(env, receiver) != Some("LoadingScreen")
        || selector_name != "loadFarmScene"
        || std::env::var("TOUCHHLE_ZF2_HOST_LOAD_SCENE")
            .ok()
            .as_deref()
            != Some("1")
    {
        return false;
    }

    let regs = *env.cpu.regs();
    let game_data = zombie_farm_get_game_data(env).unwrap_or(nil);
    let scene_class = env.objc.get_known_class("ZFFarmGameScene", &mut env.mem);
    let scene: id = if let Some(node_selector) = env.objc.lookup_selector("node") {
        if env
            .objc
            .object_has_method(&env.mem, scene_class, node_selector)
        {
            msg_send_no_type_checking(env, (scene_class, node_selector))
        } else {
            nil
        }
    } else {
        nil
    };
    let scene: id = if scene != nil {
        scene
    } else {
        let Some(alloc_selector) = env.objc.lookup_selector("alloc") else {
            return false;
        };
        let Some(init_selector) = env.objc.lookup_selector("init") else {
            return false;
        };
        let allocated_scene: id = msg_send_no_type_checking(env, (scene_class, alloc_selector));
        if allocated_scene == nil {
            nil
        } else {
            msg_send_no_type_checking(env, (allocated_scene, init_selector))
        }
    };
    if scene == nil {
        env.cpu.regs_mut().copy_from_slice(&regs);
        return false;
    }

    if game_data != nil {
        if let Some(load_scene_selector) = env.objc.lookup_selector("loadSceneWithGameData:") {
            if env
                .objc
                .object_has_method(&env.mem, scene, load_scene_selector)
            {
                let _: () = msg_send_no_type_checking(env, (scene, load_scene_selector, game_data));
            }
        }
    }
    if let Some(startup_selector) = env.objc.lookup_selector("startup") {
        if env
            .objc
            .object_has_method(&env.mem, scene, startup_selector)
        {
            let _: () = msg_send_no_type_checking(env, (scene, startup_selector));
        }
    }

    let director_class = env.objc.get_known_class("CCDirector", &mut env.mem);
    if let Some(shared_director_selector) = env.objc.lookup_selector("sharedDirector") {
        let director: id =
            msg_send_no_type_checking(env, (director_class, shared_director_selector));
        if director != nil {
            let running_scene =
                if let Some(running_scene_selector) = env.objc.lookup_selector("runningScene") {
                    if env
                        .objc
                        .object_has_method(&env.mem, director, running_scene_selector)
                    {
                        msg_send_no_type_checking(env, (director, running_scene_selector))
                    } else {
                        nil
                    }
                } else {
                    nil
                };
            let scene_selector_name = if running_scene == nil {
                "runWithScene:"
            } else {
                "replaceScene:"
            };
            if let Some(scene_selector) = env.objc.lookup_selector(scene_selector_name) {
                if env
                    .objc
                    .object_has_method(&env.mem, director, scene_selector)
                {
                    let _: () = msg_send_no_type_checking(env, (director, scene_selector, scene));
                }
            }
        }
    }

    let tile_map = zombie_farm_get_farm_tile_map(env).unwrap_or(nil);
    let main_menu = zombie_farm_last_main_menu();
    let _ = zombie_farm_remove_view_controller_view(env, receiver, "LoadingScreen");
    if let Some(main_menu) = main_menu {
        let _ = zombie_farm_remove_view_controller_view(env, main_menu, "MainMenu");
    }
    zombie_farm_reveal_hud_controls(env, "host loadFarmScene");
    zombie_farm_finish_skipped_startup_sync(env);
    env.cpu.regs_mut().copy_from_slice(&regs);
    env.cpu.regs_mut()[0] = 0;
    log!(
        "ZombieFarm2 workaround: host-loaded farm scene {:?}, tile map {:?}",
        scene,
        tile_map
    );
    true
}

fn zombie_farm_skip_farmer_head_modal(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2"
        || zombie_farm_object_class_name(env, receiver) != Some("FarmerHeadMenu")
        || selector_name != "open"
    {
        return false;
    }

    let regs = *env.cpu.regs();
    if let Some(select_selector) = env.objc.lookup_selector("selectHeadIndex:") {
        if env
            .objc
            .object_has_method(&env.mem, receiver, select_selector)
        {
            let _: () = msg_send_no_type_checking(env, (receiver, select_selector, 0i32));
        }
    }
    env.cpu.regs_mut().copy_from_slice(&regs);

    let regs = *env.cpu.regs();
    if let Some(selected_selector) = env.objc.lookup_selector("headSelected") {
        if env
            .objc
            .object_has_method(&env.mem, receiver, selected_selector)
        {
            let _: () = msg_send_no_type_checking(env, (receiver, selected_selector));
        }
    }
    env.cpu.regs_mut().copy_from_slice(&regs);

    let _ = zombie_farm_remove_view_controller_view(env, receiver, "FarmerHeadMenu");
    zombie_farm_remove_stale_uikit_subviews_by_class(env, "WhiteDimLayer");
    zombie_farm_reveal_hud_controls(env, "FarmerHeadMenu");
    zombie_farm_finish_skipped_startup_sync(env);
    env.cpu.regs_mut()[0] = 0;
    log!("ZombieFarm2 workaround: selected default farmer head and skipped FarmerHeadMenu open");
    true
}

fn zombie_farm_skip_cocos_denshion_effects(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2" {
        return false;
    }

    let Some(class_name) = zombie_farm_object_class_name(env, receiver).map(str::to_string) else {
        return false;
    };

    let should_skip = if class_name == "SimpleAudioEngine" {
        matches!(
            selector_name,
            "playEffect:"
                | "playEffect:pitch:pan:gain:"
                | "preloadEffect:"
                | "unloadEffect:"
                | "stopEffect:"
        )
    } else if class_name == "CDBufferManager" {
        matches!(
            selector_name,
            "bufferForFile:create:" | "releaseBufferForFile:"
        )
    } else if class_name == "CDSoundEngine" {
        matches!(
            selector_name,
            "loadBuffer:filePath:"
                | "loadBufferFromData:soundData:format:size:freq:"
                | "playSound:sourceGroupId:pitch:pan:gain:loop:"
                | "stopSound:"
                | "stopAllSounds"
        )
    } else {
        false
    };

    if !should_skip {
        return false;
    }

    env.cpu.regs_mut()[0] = 0;
    log!(
        "ZombieFarm2 workaround: skipping CocosDenshion effect [{} {}]",
        class_name,
        selector_name
    );
    true
}

fn zombie_farm_route_eagl_view_touches(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2"
        || zombie_farm_object_class_name(env, receiver) != Some("EAGLView")
        || !matches!(
            selector_name,
            "touchesBegan:withEvent:"
                | "touchesMoved:withEvent:"
                | "touchesEnded:withEvent:"
                | "touchesCancelled:withEvent:"
        )
    {
        return false;
    }

    let dispatcher_class = env.objc.get_known_class("CCTouchDispatcher", &mut env.mem);
    let Some(shared_dispatcher_selector) = env.objc.lookup_selector("sharedDispatcher") else {
        return false;
    };
    if !env
        .objc
        .object_has_method(&env.mem, dispatcher_class, shared_dispatcher_selector)
    {
        return false;
    }
    let Some(touch_selector) = env.objc.lookup_selector(selector_name) else {
        return false;
    };

    let touches = id::from_bits(env.cpu.regs()[2]);
    let event = id::from_bits(env.cpu.regs()[3]);
    let regs = *env.cpu.regs();
    let dispatcher: id =
        msg_send_no_type_checking(env, (dispatcher_class, shared_dispatcher_selector));
    env.cpu.regs_mut().copy_from_slice(&regs);
    if dispatcher == nil
        || !env
            .objc
            .object_has_method(&env.mem, dispatcher, touch_selector)
    {
        return false;
    }

    let regs = *env.cpu.regs();
    let _: () = msg_send_no_type_checking(env, (dispatcher, touch_selector, touches, event));
    env.cpu.regs_mut().copy_from_slice(&regs);
    env.cpu.regs_mut()[0] = 0;
    log!(
        "ZombieFarm2 workaround: routed [EAGLView {}] to CCTouchDispatcher {:?}",
        selector_name,
        dispatcher
    );
    true
}

fn zombie_farm_host_cocos_touch_dispatcher(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2"
        || zombie_farm_object_class_name(env, receiver) != Some("CCTouchDispatcher")
    {
        return false;
    }

    match selector_name {
        "addTargetedDelegate:priority:swallowsTouches:" => {
            let delegate = id::from_bits(env.cpu.regs()[2]);
            if zombie_farm_object_pointer_looks_valid(env, delegate) {
                let mut handlers = zombie_farm_cocos_touch_handlers().lock().unwrap();
                let entry = handlers.entry(receiver.to_bits()).or_default();
                let delegate_bits = delegate.to_bits();
                if !entry.targeted.contains(&delegate_bits) {
                    entry.targeted.push(delegate_bits);
                }
                log!(
                    "ZombieFarm2 workaround: host CCTouchDispatcher add targeted delegate {:?} ({})",
                    delegate,
                    zombie_farm_object_class_name(env, delegate).unwrap_or("unknown")
                );
            }
            env.cpu.regs_mut()[0] = 0;
            true
        }
        "addStandardDelegate:priority:" => {
            let delegate = id::from_bits(env.cpu.regs()[2]);
            if zombie_farm_object_pointer_looks_valid(env, delegate) {
                let mut handlers = zombie_farm_cocos_touch_handlers().lock().unwrap();
                let entry = handlers.entry(receiver.to_bits()).or_default();
                let delegate_bits = delegate.to_bits();
                if !entry.standard.contains(&delegate_bits) {
                    entry.standard.push(delegate_bits);
                }
                log!(
                    "ZombieFarm2 workaround: host CCTouchDispatcher add standard delegate {:?} ({})",
                    delegate,
                    zombie_farm_object_class_name(env, delegate).unwrap_or("unknown")
                );
            }
            env.cpu.regs_mut()[0] = 0;
            true
        }
        "removeDelegate:" => {
            let delegate_bits = env.cpu.regs()[2];
            let mut handlers = zombie_farm_cocos_touch_handlers().lock().unwrap();
            if let Some(entry) = handlers.get_mut(&receiver.to_bits()) {
                entry.targeted.retain(|&value| value != delegate_bits);
                entry.standard.retain(|&value| value != delegate_bits);
                for claimed in entry.claimed_targeted.values_mut() {
                    claimed.retain(|&value| value != delegate_bits);
                }
            }
            env.cpu.regs_mut()[0] = 0;
            true
        }
        "removeAllDelegates" => {
            zombie_farm_cocos_touch_handlers()
                .lock()
                .unwrap()
                .remove(&receiver.to_bits());
            env.cpu.regs_mut()[0] = 0;
            true
        }
        "dispatchEvents" => {
            env.cpu.regs_mut()[0] = 1;
            true
        }
        "setDispatchEvents:" => {
            env.cpu.regs_mut()[0] = 0;
            true
        }
        "touchesBegan:withEvent:"
        | "touchesMoved:withEvent:"
        | "touchesEnded:withEvent:"
        | "touchesCancelled:withEvent:" => {
            zombie_farm_dispatch_cocos_touches(env, receiver, selector_name);
            env.cpu.regs_mut()[0] = 0;
            true
        }
        "touches:withEvent:withTouchType:" => {
            env.cpu.regs_mut()[0] = 0;
            log!("ZombieFarm2 workaround: ignoring raw CCTouchDispatcher touch multiplexer");
            true
        }
        _ => false,
    }
}

fn zombie_farm_dispatch_cocos_touches(env: &mut Environment, dispatcher: id, selector_name: &str) {
    let mut handlers = zombie_farm_cocos_touch_handlers()
        .lock()
        .unwrap()
        .get(&dispatcher.to_bits())
        .cloned()
        .unwrap_or_default();

    let regs = *env.cpu.regs();
    let fallback_touch_delegate = zombie_farm_get_farm_tile_map(env);
    env.cpu.regs_mut().copy_from_slice(&regs);
    if let Some(touch_delegate) = fallback_touch_delegate {
        let delegate_bits = touch_delegate.to_bits();
        if !handlers.targeted.contains(&delegate_bits) {
            log!(
                "ZombieFarm2 workaround: adding {:?} ({}) as fallback Cocos touch delegate",
                touch_delegate,
                zombie_farm_object_class_name(env, touch_delegate).unwrap_or("unknown")
            );
            handlers.targeted.push(delegate_bits);
        }
    }

    if handlers.targeted.is_empty() && handlers.standard.is_empty() {
        log!(
            "ZombieFarm2 workaround: no Cocos touch delegates for {}",
            selector_name
        );
        return;
    }

    let touches = id::from_bits(env.cpu.regs()[2]);
    let event = id::from_bits(env.cpu.regs()[3]);
    let Some(any_object_selector) = env.objc.lookup_selector("anyObject") else {
        return;
    };
    let regs = *env.cpu.regs();
    let touch: id = msg_send_no_type_checking(env, (touches, any_object_selector));
    env.cpu.regs_mut().copy_from_slice(&regs);
    if touch == nil {
        return;
    }
    let touch_bits = touch.to_bits();
    let touch_trace_enabled = zombie_farm_touch_trace_enabled();
    if touch_trace_enabled {
        if let Some(location_selector) = env.objc.lookup_selector("locationInView:") {
            let regs = *env.cpu.regs();
            let location: CGPoint = msg_send_no_type_checking(env, (touch, location_selector, nil));
            env.cpu.regs_mut().copy_from_slice(&regs);
            log!(
                "ZombieFarm2 touch trace: dispatch {} touch {:?} location {} targeted={} standard={}",
                selector_name,
                touch,
                location,
                handlers.targeted.len(),
                handlers.standard.len(),
            );
        }
    }

    let (targeted_selector_name, standard_selector_name, targeted_returns_bool) =
        match selector_name {
            "touchesBegan:withEvent:" => {
                ("ccTouchBegan:withEvent:", "ccTouchesBegan:withEvent:", true)
            }
            "touchesMoved:withEvent:" => (
                "ccTouchMoved:withEvent:",
                "ccTouchesMoved:withEvent:",
                false,
            ),
            "touchesEnded:withEvent:" => (
                "ccTouchEnded:withEvent:",
                "ccTouchesEnded:withEvent:",
                false,
            ),
            "touchesCancelled:withEvent:" => (
                "ccTouchCancelled:withEvent:",
                "ccTouchesCancelled:withEvent:",
                false,
            ),
            _ => return,
        };

    if let Some(targeted_selector) = env.objc.lookup_selector(targeted_selector_name) {
        let targeted_delegates = if selector_name == "touchesBegan:withEvent:" {
            handlers.targeted.clone()
        } else {
            let claimed = handlers
                .claimed_targeted
                .get(&touch_bits)
                .cloned()
                .unwrap_or_default();
            if claimed.is_empty() {
                log_dbg!(
                    "ZombieFarm2 workaround: no claimed Cocos touch delegates for {}, falling back to registered targeted delegates",
                    selector_name
                );
                handlers.targeted.clone()
            } else {
                claimed
            }
        };
        let mut claimed_delegates = Vec::new();
        for delegate_bits in targeted_delegates {
            let delegate = id::from_bits(delegate_bits);
            if !zombie_farm_object_pointer_looks_valid(env, delegate)
                || !env
                    .objc
                    .object_has_method(&env.mem, delegate, targeted_selector)
            {
                continue;
            }
            let regs = *env.cpu.regs();
            if targeted_returns_bool {
                let claimed: bool =
                    msg_send_no_type_checking(env, (delegate, targeted_selector, touch, event));
                if claimed {
                    claimed_delegates.push(delegate_bits);
                }
                log_dbg!(
                    "ZombieFarm2 workaround: [{} ccTouchBegan] returned {}",
                    zombie_farm_object_class_name(env, delegate).unwrap_or("unknown"),
                    claimed
                );
                if touch_trace_enabled {
                    log!(
                        "ZombieFarm2 touch trace: [{} ccTouchBegan] returned {}",
                        zombie_farm_object_class_name(env, delegate).unwrap_or("unknown"),
                        claimed
                    );
                }
            } else {
                let _: () =
                    msg_send_no_type_checking(env, (delegate, targeted_selector, touch, event));
                if touch_trace_enabled {
                    log!(
                        "ZombieFarm2 touch trace: sent [{} {}]",
                        zombie_farm_object_class_name(env, delegate).unwrap_or("unknown"),
                        targeted_selector_name
                    );
                }
            }
            env.cpu.regs_mut().copy_from_slice(&regs);
        }
        if selector_name == "touchesBegan:withEvent:" {
            let mut all_handlers = zombie_farm_cocos_touch_handlers().lock().unwrap();
            let entry = all_handlers.entry(dispatcher.to_bits()).or_default();
            if claimed_delegates.is_empty() {
                entry.claimed_targeted.remove(&touch_bits);
            } else {
                entry.claimed_targeted.insert(touch_bits, claimed_delegates);
            }
        } else if matches!(
            selector_name,
            "touchesEnded:withEvent:" | "touchesCancelled:withEvent:"
        ) {
            let mut all_handlers = zombie_farm_cocos_touch_handlers().lock().unwrap();
            if let Some(entry) = all_handlers.get_mut(&dispatcher.to_bits()) {
                entry.claimed_targeted.remove(&touch_bits);
            }
        }
    }

    if let Some(standard_selector) = env.objc.lookup_selector(standard_selector_name) {
        for delegate_bits in handlers.standard {
            let delegate = id::from_bits(delegate_bits);
            if !zombie_farm_object_pointer_looks_valid(env, delegate)
                || !env
                    .objc
                    .object_has_method(&env.mem, delegate, standard_selector)
            {
                continue;
            }
            let regs = *env.cpu.regs();
            let _: () =
                msg_send_no_type_checking(env, (delegate, standard_selector, touches, event));
            env.cpu.regs_mut().copy_from_slice(&regs);
        }
    }
}

fn zombie_farm_skip_unsafe_cocos_touch_dispatch(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2"
        || !matches!(selector_name, "touchesCancelled:withEvent:")
        || zombie_farm_object_class_name(env, receiver) != Some("CCTouchDispatcher")
    {
        return false;
    }

    env.cpu.regs_mut()[0] = 0;
    log!(
        "ZombieFarm2 workaround: skipping unsafe CCTouchDispatcher {}",
        selector_name
    );
    true
}

fn zombie_farm_trace_game_interaction_message(
    env: &Environment,
    receiver: id,
    selector_name: &str,
) {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2" {
        return;
    }

    if !zombie_farm_touch_trace_enabled() {
        return;
    }

    if !matches!(
        selector_name,
        "tileTapped:"
            | "actorTapped:"
            | "touchedTileNodeForLocation:checkDimensions:"
            | "tileFromScreenPoint:considerOffMap:"
            | "validTile:"
            | "touched:checkDimensions:"
            | "onTileClickUp:forTool:"
            | "onActorClickUp:forTool:"
            | "attemptToPlaceItemAtPoint:"
            | "placeToolOnNearestTileToPointOnMap:forTool:"
            | "pushGameAction:onTile:withRect:withItem:"
            | "popGameActionAndExecute:"
            | "setCurrentPlayerAction:"
            | "currentPlayerAction"
            | "clearMap"
            | "harvestPlantCropAt:"
            | "harvestZombieCropAt:"
            | "isPlantCrop"
            | "isZombieCrop"
            | "isHarvestable"
            | "ready"
            | "timeLeftToHarvestCropTile:"
            | "isTilePlantable:"
            | "canPlaceTileOfSize:at:"
            | "tutorialZombieHarvested"
            | "selectTool:withLabel:withImage:"
            | "selectTool:withLabel:"
            | "toolSelected:"
            | "currentGameTool"
            | "setCurrentGameTool:"
            | "toolCleanup:"
            | "payForItemFromTool:"
            | "deductResourceForAction:"
            | "clearAllGameActions"
            | "menuTapped"
            | "marketTapped"
            | "itemTapped:"
            | "questJournalTapped"
            | "goldBarTapped"
            | "brainsBarTapped"
    ) {
        return;
    }

    let stack_detail = if selector_name == "onTileClickUp:forTool:" {
        let stack_tool: u32 = env.mem.read(ConstPtr::from_bits(env.cpu.regs()[Cpu::SP]));
        format!(" stack_tool={}", stack_tool)
    } else {
        String::new()
    };

    log!(
        "ZombieFarm2 touch trace: [{} {}] r2=0x{:x} r3=0x{:x}{}",
        zombie_farm_object_class_name(env, receiver).unwrap_or("unknown"),
        selector_name,
        env.cpu.regs()[2],
        env.cpu.regs()[3],
        stack_detail,
    );
}

fn zombie_farm_force_status_bar_timeout(env: &mut Environment, receiver: id, selector_name: &str) {
    if !env
        .bundle
        .bundle_identifier()
        .starts_with("com.playforge.Z")
    {
        return;
    }

    let is_status_bar_message = selector_name == "statusMessage:cancelAfter:";
    let is_status_bar_view_message = zombie_farm_object_class_name(env, receiver)
        == Some("StatusBar")
        && matches!(
            selector_name,
            "showMessage:withCancelTimeout:andCancelNotification:"
                | "updateMessage:andCancelTimeout:andCancelNotification:"
        );
    if !is_status_bar_message && !is_status_bar_view_message {
        return;
    }

    let regs = *env.cpu.regs();
    let timeout: id = msg_class![env; NSNumber numberWithFloat:1.0f32];
    env.cpu.regs_mut().copy_from_slice(&regs);
    env.cpu.regs_mut()[3] = timeout.to_bits();
    log!(
        "ZombieFarm workaround: forcing [StatusBar {}] timeout to 1s",
        selector_name
    );
    zombie_farm_schedule_status_bar_hide(env, receiver);
}

fn zombie_farm_schedule_delayed_noarg_selector(
    env: &mut Environment,
    target: id,
    selector_name: &str,
    delay: f64,
) -> bool {
    if target == nil {
        return false;
    }
    let Some(selector) = env.objc.lookup_selector(selector_name) else {
        return false;
    };
    if !env.objc.object_has_method(&env.mem, target, selector) {
        return false;
    }
    let Some(delayed_selector) = env
        .objc
        .lookup_selector("performSelector:withObject:afterDelay:")
    else {
        return false;
    };
    if !env
        .objc
        .object_has_method(&env.mem, target, delayed_selector)
    {
        return false;
    }

    let regs = *env.cpu.regs();
    let _: () = msg_send_no_type_checking(env, (target, delayed_selector, selector, nil, delay));
    env.cpu.regs_mut().copy_from_slice(&regs);
    true
}

fn zombie_farm_schedule_status_bar_hide(env: &mut Environment, receiver: id) {
    let regs = *env.cpu.regs();
    let status_bar_class = env.objc.get_known_class("StatusBar", &mut env.mem);
    let status_bar = if let Some(status_bar_selector) = env.objc.lookup_selector("statusBar") {
        if env
            .objc
            .object_has_method(&env.mem, status_bar_class, status_bar_selector)
        {
            msg_send_no_type_checking(env, (status_bar_class, status_bar_selector))
        } else {
            nil
        }
    } else {
        nil
    };
    let status_bar =
        if status_bar == nil && zombie_farm_object_class_name(env, receiver) == Some("StatusBar") {
            receiver
        } else {
            status_bar
        };
    env.cpu.regs_mut().copy_from_slice(&regs);

    let scheduled_hide = zombie_farm_schedule_delayed_noarg_selector(env, status_bar, "hide", 1.0);

    let view = if status_bar != nil {
        let regs = *env.cpu.regs();
        let view = if let Some(view_selector) = env.objc.lookup_selector("view") {
            if env
                .objc
                .object_has_method(&env.mem, status_bar, view_selector)
            {
                msg_send_no_type_checking(env, (status_bar, view_selector))
            } else {
                nil
            }
        } else {
            nil
        };
        env.cpu.regs_mut().copy_from_slice(&regs);
        view
    } else {
        nil
    };
    let scheduled_remove =
        zombie_farm_schedule_delayed_noarg_selector(env, view, "removeFromSuperview", 1.0);

    log!(
        "ZombieFarm workaround: scheduled StatusBar cleanup in 1s (hide={}, removeView={})",
        scheduled_hide,
        scheduled_remove
    );
}

fn zombie_farm_trace_game_interaction_return(env: &Environment, receiver: id, selector_name: &str) {
    if env.bundle.bundle_identifier() != "com.playforge.ZombieFarm2"
        || !zombie_farm_touch_trace_enabled()
        || !matches!(
            zombie_farm_object_class_name(env, receiver),
            Some("ZFFarmTileMap" | "ZFTileManager" | "ZFToolManager" | "Tile")
        )
        || !matches!(
            selector_name,
            "touchedTileNodeForLocation:checkDimensions:"
                | "tileFromScreenPoint:considerOffMap:"
                | "validTile:"
                | "touched:checkDimensions:"
                | "harvestPlantCropAt:"
                | "harvestZombieCropAt:"
                | "isPlantCrop"
                | "isZombieCrop"
                | "isHarvestable"
                | "ready"
                | "timeLeftToHarvestCropTile:"
                | "isTilePlantable:"
                | "canPlaceTileOfSize:at:"
                | "toolSelected:"
        )
    {
        return;
    }

    log!(
        "ZombieFarm2 touch trace: [{} {}] -> r0=0x{:x}",
        zombie_farm_object_class_name(env, receiver).unwrap_or("unknown"),
        selector_name,
        env.cpu.regs()[0],
    );
}

fn zombie_farm_actor_hunger(env: &mut Environment, actor: id) -> Option<f32> {
    if actor == nil {
        return None;
    }

    if let Some(hunger_selector) = env.objc.lookup_selector("hunger") {
        if env.objc.object_has_method(&env.mem, actor, hunger_selector) {
            return Some(msg_send_no_type_checking(env, (actor, hunger_selector)));
        }
    }

    let ivar_name = "hunger".to_string();
    let ivar = env.objc.object_lookup_ivar(&env.mem, actor, &ivar_name)?;
    Some(env.mem.read(ivar.cast()))
}

fn zombie_farm_set_actor_hunger(env: &mut Environment, actor: id, hunger: f32) -> bool {
    if actor == nil {
        return false;
    }

    if zombie_farm_actor_is_zombie(env, actor) {
        return zombie_farm_write_actor_hunger_ivar(env, actor, hunger.max(1.0).clamp(0.0, 1.0));
    }

    if let Some(set_hunger_selector) = env.objc.lookup_selector("setHunger:") {
        if env
            .objc
            .object_has_method(&env.mem, actor, set_hunger_selector)
        {
            let _: () = msg_send_no_type_checking(env, (actor, set_hunger_selector, hunger));
            return true;
        }
    }

    zombie_farm_write_actor_hunger_ivar(env, actor, hunger)
}

fn zombie_farm_force_zombie_hunger_in_list(env: &mut Environment, actor_list: id) -> (u32, u32) {
    let Some(count_selector) = env.objc.lookup_selector("count") else {
        return (0, 0);
    };
    let Some(object_at_index_selector) = env.objc.lookup_selector("objectAtIndex:") else {
        return (0, 0);
    };
    if !env
        .objc
        .object_has_method(&env.mem, actor_list, count_selector)
        || !env
            .objc
            .object_has_method(&env.mem, actor_list, object_at_index_selector)
    {
        return (0, 0);
    }

    let count: NSUInteger = msg_send_no_type_checking(env, (actor_list, count_selector));
    let mut zombies = 0u32;
    let mut changed = 0u32;
    for idx in 0..count {
        let actor: id = msg_send_no_type_checking(env, (actor_list, object_at_index_selector, idx));
        if actor == nil || !zombie_farm_actor_is_zombie(env, actor) {
            continue;
        }
        zombies += 1;
        let old_hunger = zombie_farm_read_actor_hunger_ivar(env, actor).unwrap_or(0.0);
        if old_hunger < 0.999 && zombie_farm_write_actor_hunger_ivar(env, actor, 1.0) {
            changed += 1;
        }
    }
    (zombies, changed)
}

fn zombie_farm_force_all_zombie_hunger(env: &mut Environment, reason: &str) {
    if !zombie_farm_uses_playforge_bundle(env) {
        return;
    }

    let mut zombies = 0u32;
    let mut changed = 0u32;
    for actor_list in [
        zombie_farm_get_actor_list(env),
        zombie_farm_get_live_actor_list(env),
    ]
    .into_iter()
    .flatten()
    {
        let (list_zombies, list_changed) = zombie_farm_force_zombie_hunger_in_list(env, actor_list);
        zombies += list_zombies;
        changed += list_changed;
    }

    if zombies > 0 && changed > 0 {
        log_dbg!(
            "ZombieFarm status: force-filled hunger for {}/{} zombie actor entries before {}",
            changed,
            zombies,
            reason
        );
    }
}

fn zombie_farm_force_zombie_hunger_message(
    env: &mut Environment,
    receiver: id,
    selector_name: &str,
) -> bool {
    if !zombie_farm_uses_playforge_bundle(env) {
        return false;
    }

    if selector_name == "getAverageHunger" {
        zombie_farm_force_all_zombie_hunger(env, "getAverageHunger");
        env.cpu.regs_mut()[0] = 1.0f32.to_bits();
        log_dbg!("ZombieFarm status: [getAverageHunger] -> 1.000");
        return true;
    }

    if !zombie_farm_actor_is_zombie(env, receiver) {
        return false;
    }

    match selector_name {
        "hunger" => {
            zombie_farm_write_actor_hunger_ivar(env, receiver, 1.0);
            env.cpu.regs_mut()[0] = 1.0f32.to_bits();
            true
        }
        "setHunger:" => {
            zombie_farm_write_actor_hunger_ivar(env, receiver, 1.0);
            env.cpu.regs_mut()[0] = 0;
            true
        }
        _ => false,
    }
}

fn zombie_farm_zombie_hungers_in_list(env: &mut Environment, actor_list: id) -> Vec<f32> {
    let Some(count_selector) = env.objc.lookup_selector("count") else {
        return Vec::new();
    };
    let Some(object_at_index_selector) = env.objc.lookup_selector("objectAtIndex:") else {
        return Vec::new();
    };
    if !env
        .objc
        .object_has_method(&env.mem, actor_list, count_selector)
        || !env
            .objc
            .object_has_method(&env.mem, actor_list, object_at_index_selector)
    {
        return Vec::new();
    }

    let count: NSUInteger = msg_send_no_type_checking(env, (actor_list, count_selector));
    let mut hungers = Vec::new();
    for idx in 0..count {
        let actor: id = msg_send_no_type_checking(env, (actor_list, object_at_index_selector, idx));
        if actor == nil || !zombie_farm_actor_is_zombie(env, actor) {
            continue;
        }
        hungers.push(zombie_farm_actor_hunger(env, actor).unwrap_or(0.0));
    }
    hungers
}

fn zombie_farm_apply_zombie_hungers_to_list(
    env: &mut Environment,
    actor_list: id,
    target_hungers: &[f32],
) -> (u32, u32, f32) {
    let Some(count_selector) = env.objc.lookup_selector("count") else {
        return (0, 0, 0.0);
    };
    let Some(object_at_index_selector) = env.objc.lookup_selector("objectAtIndex:") else {
        return (0, 0, 0.0);
    };
    if !env
        .objc
        .object_has_method(&env.mem, actor_list, count_selector)
        || !env
            .objc
            .object_has_method(&env.mem, actor_list, object_at_index_selector)
    {
        return (0, 0, 0.0);
    }

    let count: NSUInteger = msg_send_no_type_checking(env, (actor_list, count_selector));
    let mut zombies = 0u32;
    let mut changed = 0u32;
    let mut max_hunger = 0.0f32;
    for idx in 0..count {
        let actor: id = msg_send_no_type_checking(env, (actor_list, object_at_index_selector, idx));
        if actor == nil || !zombie_farm_actor_is_zombie(env, actor) {
            continue;
        }

        let Some(&target_hunger) = target_hungers.get(zombies as usize) else {
            break;
        };
        zombies += 1;
        let old_hunger = zombie_farm_actor_hunger(env, actor).unwrap_or(0.0);
        let new_hunger = old_hunger.max(target_hunger).clamp(0.0, 1.0);
        if new_hunger > old_hunger + 0.001 && zombie_farm_set_actor_hunger(env, actor, new_hunger) {
            changed += 1;
        }
        max_hunger = max_hunger.max(new_hunger);
    }

    (zombies, changed, max_hunger)
}

fn zombie_farm_sync_actor_hunger_lists(env: &mut Environment) -> (u32, u32, f32) {
    let lists = [
        zombie_farm_get_actor_list(env),
        zombie_farm_get_live_actor_list(env),
    ];
    let mut target_hungers = Vec::new();
    for actor_list in lists.into_iter().flatten() {
        for (idx, hunger) in zombie_farm_zombie_hungers_in_list(env, actor_list)
            .into_iter()
            .enumerate()
        {
            if idx == target_hungers.len() {
                target_hungers.push(hunger);
            } else if let Some(target) = target_hungers.get_mut(idx) {
                *target = (*target).max(hunger);
            }
        }
    }
    if target_hungers.is_empty() {
        return (0, 0, 0.0);
    }

    let mut zombies = 0u32;
    let mut changed = 0u32;
    let mut max_hunger = 0.0f32;
    for actor_list in [
        zombie_farm_get_actor_list(env),
        zombie_farm_get_live_actor_list(env),
    ]
    .into_iter()
    .flatten()
    {
        let (list_zombies, list_changed, list_max_hunger) =
            zombie_farm_apply_zombie_hungers_to_list(env, actor_list, &target_hungers);
        zombies += list_zombies;
        changed += list_changed;
        max_hunger = max_hunger.max(list_max_hunger);
    }

    if changed > 0 {
        log_dbg!(
            "ZombieFarm status: synced hunger for {}/{} zombie actor entries, max hunger {:.3}",
            changed,
            zombies,
            max_hunger
        );
    }
    (zombies, changed, max_hunger)
}

fn zombie_farm_zombie_eat_dates_near(
    env: &mut Environment,
    actor_list: id,
    date_interval: f64,
    tolerance: f64,
) -> (u32, u32) {
    let Some(count_selector) = env.objc.lookup_selector("count") else {
        return (0, 0);
    };
    let Some(object_at_index_selector) = env.objc.lookup_selector("objectAtIndex:") else {
        return (0, 0);
    };
    if !env
        .objc
        .object_has_method(&env.mem, actor_list, count_selector)
        || !env
            .objc
            .object_has_method(&env.mem, actor_list, object_at_index_selector)
    {
        return (0, 0);
    }

    let count: NSUInteger = msg_send_no_type_checking(env, (actor_list, count_selector));
    let mut zombies = 0u32;
    let mut near = 0u32;
    for idx in 0..count {
        let actor: id = msg_send_no_type_checking(env, (actor_list, object_at_index_selector, idx));
        if actor == nil || !zombie_farm_actor_is_zombie(env, actor) {
            continue;
        }
        zombies += 1;
        if let Some(eat_date) = zombie_farm_actor_eat_date(env, actor) {
            if let Some(eat_interval) = zombie_farm_date_interval(env, eat_date) {
                if (eat_interval - date_interval).abs() <= tolerance {
                    near += 1;
                }
            }
        }
    }
    (zombies, near)
}

fn zombie_farm_apply_offline_actor_hunger_to_list(
    env: &mut Environment,
    actor_list: id,
    hunger_delta: f32,
    now: id,
    time_interval_since_date_selector: SEL,
    source_hungers: Option<&[f32]>,
) -> (u32, u32, f32) {
    let Some(count_selector) = env.objc.lookup_selector("count") else {
        return (0, 0, 0.0);
    };
    let Some(object_at_index_selector) = env.objc.lookup_selector("objectAtIndex:") else {
        return (0, 0, 0.0);
    };

    if !env
        .objc
        .object_has_method(&env.mem, actor_list, count_selector)
        || !env
            .objc
            .object_has_method(&env.mem, actor_list, object_at_index_selector)
    {
        return (0, 0, 0.0);
    }

    let count: NSUInteger = msg_send_no_type_checking(env, (actor_list, count_selector));
    let mut zombies = 0u32;
    let mut changed = 0u32;
    let mut max_hunger = 0.0f32;

    for idx in 0..count {
        let actor: id = msg_send_no_type_checking(env, (actor_list, object_at_index_selector, idx));
        if actor == nil || !zombie_farm_actor_is_zombie(env, actor) {
            continue;
        }
        let zombie_idx = zombies as usize;
        zombies += 1;

        let old_hunger = source_hungers
            .and_then(|hungers| hungers.get(zombie_idx).copied())
            .or_else(|| zombie_farm_actor_hunger(env, actor))
            .unwrap_or(0.0);
        let eat_date_hunger = if old_hunger <= 0.001 {
            zombie_farm_actor_eat_date(env, actor).and_then(|eat_date| {
                if eat_date == nil
                    || !env
                        .objc
                        .object_has_method(&env.mem, now, time_interval_since_date_selector)
                {
                    return None;
                }
                let elapsed_since_eat: f64 = msg_send_no_type_checking(
                    env,
                    (now, time_interval_since_date_selector, eat_date),
                );
                (elapsed_since_eat > 1.0 && elapsed_since_eat.is_finite())
                    .then(|| (elapsed_since_eat / 86_400.0).clamp(0.0, 1.0) as f32)
            })
        } else {
            None
        };
        let new_hunger = (old_hunger + hunger_delta)
            .max(eat_date_hunger.unwrap_or(0.0))
            .clamp(0.0, 1.0);
        if new_hunger > old_hunger + 0.001 {
            if !zombie_farm_set_actor_hunger(env, actor, new_hunger) {
                continue;
            }
            changed += 1;
        }
        max_hunger = max_hunger.max(new_hunger);
    }

    (zombies, changed, max_hunger)
}

fn zombie_farm_apply_offline_actor_hunger(env: &mut Environment) {
    static DISABLE_LOCAL_HUNGER: OnceLock<bool> = OnceLock::new();
    let disable_local_hunger = *DISABLE_LOCAL_HUNGER.get_or_init(|| {
        std::env::var("TOUCHHLE_ZF_DISABLE_LOCAL_HUNGER")
            .map(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "yes" | "YES"))
            .unwrap_or(false)
    });

    if !zombie_farm_uses_playforge_bundle(env) || disable_local_hunger {
        return;
    }

    let mut zombies = 0u32;
    let mut changed = 0u32;
    let mut max_elapsed = 0.0f64;
    let mut max_hunger = 0.0f32;

    let Some(game_state) = zombie_farm_get_game_state(env) else {
        return;
    };
    let Some(save_date) = zombie_farm_game_state_save_date(env, game_state) else {
        return;
    };
    let now: id = msg_class![env; NSDate date];
    let Some(time_interval_since_date_selector) =
        env.objc.lookup_selector("timeIntervalSinceDate:")
    else {
        return;
    };
    if !env
        .objc
        .object_has_method(&env.mem, now, time_interval_since_date_selector)
    {
        return;
    }
    let elapsed_since_save: f64 =
        msg_send_no_type_checking(env, (now, time_interval_since_date_selector, save_date));
    if !elapsed_since_save.is_finite() {
        return;
    }

    let save_date_interval = zombie_farm_date_interval(env, save_date).unwrap_or(f64::NAN);
    let saved_actor_list = zombie_farm_get_actor_list(env);
    let saved_hungers = saved_actor_list
        .map(|actor_list| zombie_farm_zombie_hungers_in_list(env, actor_list))
        .unwrap_or_default();
    let max_saved_hunger = saved_hungers.iter().copied().fold(0.0f32, f32::max);
    let (save_date_zombies, eat_dates_near_save_date) = saved_actor_list
        .map(|actor_list| {
            zombie_farm_zombie_eat_dates_near(env, actor_list, save_date_interval, 2.0)
        })
        .unwrap_or((0, 0));
    let save_date_looks_like_old_eat_date_workaround =
        save_date_zombies > 0 && eat_dates_near_save_date * 2 >= save_date_zombies;
    let already_applied_seconds = if save_date_looks_like_old_eat_date_workaround {
        f64::from(max_saved_hunger) * 86_400.0
    } else {
        0.0
    };
    let hunger_delta =
        ((elapsed_since_save - already_applied_seconds).max(0.0) / 86_400.0).clamp(0.0, 1.0) as f32;
    if let Some(actor_list) = zombie_farm_get_actor_list(env) {
        let (list_zombies, list_changed, list_max_hunger) =
            zombie_farm_apply_offline_actor_hunger_to_list(
                env,
                actor_list,
                hunger_delta,
                now,
                time_interval_since_date_selector,
                None,
            );
        zombies += list_zombies;
        changed += list_changed;
        max_elapsed = max_elapsed.max(elapsed_since_save);
        max_hunger = max_hunger.max(list_max_hunger);
    }

    if let Some(actor_list) = zombie_farm_get_live_actor_list(env) {
        let (list_zombies, list_changed, list_max_hunger) =
            zombie_farm_apply_offline_actor_hunger_to_list(
                env,
                actor_list,
                hunger_delta,
                now,
                time_interval_since_date_selector,
                (!saved_hungers.is_empty()).then_some(saved_hungers.as_slice()),
            );
        zombies += list_zombies;
        changed += list_changed;
        max_elapsed = max_elapsed.max(elapsed_since_save);
        max_hunger = max_hunger.max(list_max_hunger);
    }

    let (sync_zombies, sync_changed, sync_max_hunger) = zombie_farm_sync_actor_hunger_lists(env);
    zombies += sync_zombies;
    changed += sync_changed;
    max_hunger = max_hunger.max(sync_max_hunger);

    zombie_farm_set_game_state_save_date(env, game_state, now);
    log_dbg!(
        "ZombieFarm status: local hunger advanced {}/{} zombie actor entries by {:.0}s since saveDate ({:.3} hunger), max hunger {:.3}",
        changed,
        zombies,
        max_elapsed,
        hunger_delta,
        max_hunger
    );
}

fn zombie_farm_prepare_local_server_date(env: &mut Environment, receiver: id, selector_name: &str) {
    if !matches!(selector_name, "getServerTime" | "handleResponse:forAction:") {
        return;
    }

    let regs = *env.cpu.regs();
    let gui_layer = if zombie_farm_object_class_name(env, receiver) == Some("ZFGuiLayer") {
        Some(receiver)
    } else {
        zombie_farm_get_gui_layer(env)
    };
    if let Some(gui_layer) = gui_layer {
        zombie_farm_set_gui_layer_server_date_to_now(env, gui_layer);
    }
    env.cpu.regs_mut().copy_from_slice(&regs);
}

fn zombie_farm_apply_local_hunger_update(env: &mut Environment, receiver: id, selector_name: &str) {
    if !matches!(
        selector_name,
        "startUpChecksComplete" | "handleTimeResponse:"
    ) {
        return;
    }
    let regs = *env.cpu.regs();
    let gui_layer = if zombie_farm_object_class_name(env, receiver) == Some("ZFGuiLayer") {
        Some(receiver)
    } else {
        zombie_farm_get_gui_layer(env)
    };
    let is_gui_layer = gui_layer
        .map(|gui_layer| zombie_farm_set_gui_layer_server_date_to_now(env, gui_layer))
        .unwrap_or(false);
    if env.bundle.bundle_identifier() == "com.playforge.ZombieFarm2" {
        log_dbg!("ZombieFarm2 workaround: skipping offline hunger update during startup");
        env.cpu.regs_mut().copy_from_slice(&regs);
        return;
    }
    let Some(apply_hunger_selector) = env.objc.lookup_selector("applyZombieHunger") else {
        zombie_farm_apply_offline_actor_hunger(env);
        env.cpu.regs_mut().copy_from_slice(&regs);
        return;
    };
    if let Some(gui_layer) = gui_layer {
        if is_gui_layer
            && zombie_farm_ensure_game_state_save_date(env)
            && !ZOMBIE_FARM_APPLIED_LOCAL_HUNGER.swap(true, Ordering::Relaxed)
            && env
                .objc
                .object_has_method(&env.mem, gui_layer, apply_hunger_selector)
        {
            log_dbg!("ZombieFarm status: applying offline zombie hunger update");
            let _: () = msg_send_no_type_checking(env, (gui_layer, apply_hunger_selector));
        }
    }
    zombie_farm_apply_offline_actor_hunger(env);
    env.cpu.regs_mut().copy_from_slice(&regs);
}

fn zombie_farm_prepare_local_hunger_update(env: &mut Environment, selector_name: &str) {
    if !matches!(
        selector_name,
        "openMenu"
            | "openMenuThroughMausoleum"
            | "displayCurrentZombie"
            | "updateSelectedZombieInfo"
            | "displayHunger"
            | "table:cellTouched:"
            | "saveGame"
            | "startInvasion:"
            | "startInvasion:checkHunger:"
            | "startInvasionWithDictionary:checkHunger:"
            | "invadeButtonTapped:"
            | "switchToFightScene"
    ) {
        return;
    }

    let regs = *env.cpu.regs();
    zombie_farm_ensure_game_state_save_date(env);
    zombie_farm_apply_offline_actor_hunger(env);
    zombie_farm_force_all_zombie_hunger(env, selector_name);
    env.cpu.regs_mut().copy_from_slice(&regs);
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

fn trace_zombie_farm_status_normal_return(env: &mut Environment, receiver: id, selector: SEL) {
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
    if !trace_zombie_farm_status_message(class_name, selector_name) {
        return;
    }
    match selector_name {
        "timeIntervalSinceDate:"
        | "timeIntervalSinceNow"
        | "timeIntervalSince1970"
        | "timeIntervalSinceReferenceDate" => {
            let mut bytes = [0u8; 8];
            bytes[0..4].copy_from_slice(&env.cpu.regs()[0].to_le_bytes());
            bytes[4..8].copy_from_slice(&env.cpu.regs()[1].to_le_bytes());
            let value = f64::from_bits(u64::from_le_bytes(bytes));
            log!(
                "ZombieFarm status: [{} {}] receiver {:?} return {:.3}",
                class_name,
                selector_name,
                receiver,
                value
            );
        }
        "saveDate" | "addTimeInterval:" | "getBeginningOfTheDayFromDate:" => {
            log!(
                "ZombieFarm status: [{} {}] receiver {:?} return object {:?}",
                class_name,
                selector_name,
                receiver,
                id::from_bits(env.cpu.regs()[0])
            );
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
    if receiver.to_bits() < env.mem.null_segment_size() || receiver.to_bits() % 4 != 0 {
        let selector_name = selector.as_str(&env.mem);
        if selector_name == "compare:" {
            log_dbg!(
                "Ignoring compare: sent to invalid low ObjC pointer {:?}",
                receiver
            );
        } else {
            log!(
                "Warning: ignoring {} sent to invalid low ObjC pointer {:?}",
                selector_name,
                receiver
            );
        }
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
    trace_zombie_farm_sprite_message(env, receiver, &selector_name);
    zombie_farm_trace_game_interaction_message(env, receiver, &selector_name);
    zombie_farm_force_status_bar_timeout(env, receiver, &selector_name);
    if zombie_farm_forward_backing_array_fast_enumeration(env, receiver, selector, &selector_name) {
        return;
    }
    if zombie_farm_ignore_null_attachment_placeholder(env, receiver, &selector_name) {
        return;
    }
    if zombie_farm_force_zombie_hunger_message(env, receiver, &selector_name) {
        return;
    }
    if env.bundle.bundle_identifier() == "com.playforge.ZombieFarm2"
        && (zombie_farm_object_class_name(env, receiver) == Some("MainMenu")
            || selector_name == "playTapped")
    {
        ZOMBIE_FARM_LAST_MAIN_MENU.store(receiver.to_bits() as usize, Ordering::Relaxed);
        if selector_name == "playTapped" {
            log!(
                "ZombieFarm2 workaround: remembered {:?} ({}) as MainMenu candidate",
                receiver,
                zombie_farm_object_class_name(env, receiver).unwrap_or("unknown")
            );
        }
    }
    if let Some(result) = zombie_farm_md5sum_override(env, &selector_name) {
        env.cpu.regs_mut()[0] = result.to_bits();
        return;
    }
    if zombie_farm_disable_cctable_cell_reuse(env, receiver, &selector_name) {
        return;
    }
    zombie_farm_prepare_game_state_save_date(env, receiver, &selector_name);
    if zombie_farm_skip_epic_event_with_missing_remote_data(env, receiver, &selector_name) {
        return;
    }
    if zombie_farm_override_cocos2d_get_zeye(env, receiver, &selector_name) {
        return;
    }
    if zombie_farm_skip_cocos2d_projection_setup(env, receiver, &selector_name) {
        return;
    }
    if zombie_farm_skip_remote_asset_requests(env, receiver, &selector_name) {
        return;
    }
    if zombie_farm_skip_event_tracker_nil_last_event(env, receiver, &selector_name) {
        return;
    }
    if zombie_farm_skip_event_tracker_init(env, receiver, &selector_name) {
        return;
    }
    if zombie_farm_return_open_udid(env, receiver, &selector_name) {
        return;
    }
    if zombie_farm_skip_vungle_ad_sdk(env, receiver, &selector_name) {
        return;
    }
    if zombie_farm_skip_broken_font_preload(env, receiver, &selector_name) {
        return;
    }
    if zombie_farm_skip_brain_client_network(env, receiver, &selector_name) {
        return;
    }
    if zombie_farm_skip_sync_queue_network(env, receiver, &selector_name) {
        return;
    }
    if zombie_farm_return_safe_game_state_count(env, receiver, &selector_name) {
        return;
    }
    if zombie_farm_skip_remote_dependent_game_state_update(env, receiver, &selector_name) {
        return;
    }
    if zombie_farm_return_self_for_game_data_copy(env, receiver, &selector_name) {
        return;
    }
    if zombie_farm_host_actor_manager_init(env, receiver, &selector_name) {
        return;
    }
    if zombie_farm_skip_tool_manager_transient_actions(env, receiver, &selector_name) {
        return;
    }
    if zombie_farm_skip_quest_manager_reset(env, receiver, &selector_name) {
        return;
    }
    if zombie_farm_skip_unsafe_toolbar_build(env, receiver, &selector_name) {
        return;
    }
    if zombie_farm_skip_market_offers(env, receiver, &selector_name) {
        return;
    }
    if zombie_farm_skip_event_ad_networks(env, receiver, &selector_name) {
        return;
    }
    if zombie_farm_skip_farmer_head_modal(env, receiver, &selector_name) {
        return;
    }
    if zombie_farm_host_load_farm_scene(env, receiver, &selector_name) {
        return;
    }
    if zombie_farm_skip_startup_profile_detection(env, receiver, &selector_name) {
        return;
    }
    if zombie_farm_skip_startup_internet_loading(env, receiver, &selector_name) {
        return;
    }
    if zombie_farm_skip_cocos_denshion_effects(env, receiver, &selector_name) {
        return;
    }
    if zombie_farm_route_eagl_view_touches(env, receiver, &selector_name) {
        return;
    }
    if zombie_farm_host_cocos_touch_dispatcher(env, receiver, &selector_name) {
        return;
    }
    if zombie_farm_skip_unsafe_cocos_touch_dispatch(env, receiver, &selector_name) {
        return;
    }
    zombie_farm_prepare_local_server_date(env, receiver, &selector_name);
    zombie_farm_prepare_local_hunger_update(env, &selector_name);
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
                let selector_name_owned = selector.as_str(&env.mem).to_string();
                let selector_name = selector_name_owned.as_str();
                let receiver_class_name_owned = env
                    .objc
                    .try_get_class_name(orig_class)
                    .unwrap_or(name)
                    .to_string();
                let receiver_class_name = receiver_class_name_owned.as_str();
                let zombie_farm_debug_enabled =
                    zombie_farm_uses_playforge_bundle(env) && crate::zombie_farm_debug::enabled();
                if zombie_farm_debug_enabled {
                    crate::zombie_farm_debug::record_objc_message(
                        receiver,
                        receiver_class_name,
                        selector_name,
                        env.cpu.regs(),
                    );
                }
                let trace_zombie_farm_layout = zombie_farm_debug_enabled
                    && (trace_zombie_farm_layout_message(receiver_class_name, selector_name)
                        || trace_zombie_farm_layout_message(name, selector_name));
                let trace_zombie_farm_status = zombie_farm_debug_enabled
                    && (trace_zombie_farm_status_message(receiver_class_name, selector_name)
                        || trace_zombie_farm_status_message(name, selector_name));
                let record_zombie_farm_hunger = zombie_farm_debug_enabled
                    && (crate::zombie_farm_debug::should_record_hunger_message(
                        receiver_class_name,
                        selector_name,
                    ) || crate::zombie_farm_debug::should_record_hunger_message(
                        name,
                        selector_name,
                    ));
                let trace_zombie_farm_apply_scope = zombie_farm_debug_enabled
                    && selector_name == "applyZombieHunger"
                    && (receiver_class_name == "ZFGuiLayer" || name == "ZFGuiLayer");
                let record_zombie_farm_apply_trace = zombie_farm_debug_enabled
                    && ZOMBIE_FARM_APPLY_TRACE_DEPTH.load(Ordering::Relaxed) > 0
                    && (crate::zombie_farm_debug::should_record_apply_trace_message(
                        receiver_class_name,
                        selector_name,
                    ) || crate::zombie_farm_debug::should_record_apply_trace_message(
                        name,
                        selector_name,
                    ));
                let record_zombie_farm_cocos_label_text = zombie_farm_uses_playforge_bundle(env)
                    && (crate::zombie_farm_debug::should_record_cocos_label_text(
                        receiver_class_name,
                        selector_name,
                    ) || crate::zombie_farm_debug::should_record_cocos_label_text(
                        name,
                        selector_name,
                    ));
                if trace_zombie_farm_status || trace_zombie_farm_layout {
                    let imp_description = match imp {
                        IMP::Host(_) => "host".to_string(),
                        IMP::Guest(guest_imp) => format!("{:?}", guest_imp),
                    };
                    let arg_description = if trace_zombie_farm_status {
                        zombie_farm_status_arg_details(selector_name, env.cpu.regs())
                    } else {
                        zombie_farm_layout_arg_details(selector_name, env.cpu.regs())
                    }
                    .map(|arg| format!(" {}", arg))
                    .unwrap_or_default();
                    if trace_zombie_farm_status {
                        log!(
                            "ZombieFarm status: [{} {}] receiver {:?}, implementation class {}, imp {}{}",
                            receiver_class_name,
                            selector_name,
                            receiver,
                            name,
                            imp_description,
                            arg_description,
                        );
                    }
                    if trace_zombie_farm_layout {
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
                }
                let regs_before_zombie_farm_record = if record_zombie_farm_hunger
                    || record_zombie_farm_apply_trace
                    || record_zombie_farm_cocos_label_text
                {
                    Some(*env.cpu.regs())
                } else {
                    None
                };
                if record_zombie_farm_hunger || record_zombie_farm_apply_trace {
                    let regs_before = regs_before_zombie_farm_record.as_ref().unwrap();
                    if record_zombie_farm_hunger {
                        crate::zombie_farm_debug::record_hunger_message(
                            env,
                            receiver,
                            receiver_class_name,
                            selector_name,
                            regs_before,
                        );
                    }
                    if record_zombie_farm_apply_trace {
                        crate::zombie_farm_debug::record_apply_trace_message(
                            env,
                            receiver,
                            receiver_class_name,
                            selector_name,
                            regs_before,
                        );
                    }
                }
                let selector_name_for_after = selector_name.to_string();
                if trace_zombie_farm_apply_scope {
                    ZOMBIE_FARM_APPLY_TRACE_DEPTH.fetch_add(1, Ordering::Relaxed);
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
                if trace_zombie_farm_apply_scope {
                    ZOMBIE_FARM_APPLY_TRACE_DEPTH.fetch_sub(1, Ordering::Relaxed);
                }
                if trace_zombie_farm_layout {
                    trace_zombie_farm_layout_normal_return(env, receiver, selector);
                }
                if trace_zombie_farm_status {
                    trace_zombie_farm_status_normal_return(env, receiver, selector);
                }
                if record_zombie_farm_hunger {
                    crate::zombie_farm_debug::record_hunger_return(
                        env,
                        receiver,
                        receiver_class_name,
                        selector_name,
                    );
                }
                if record_zombie_farm_apply_trace {
                    crate::zombie_farm_debug::record_apply_trace_return(
                        env,
                        receiver,
                        receiver_class_name,
                        selector_name,
                    );
                }
                if record_zombie_farm_cocos_label_text {
                    crate::zombie_farm_debug::record_cocos_label_text_return(
                        env,
                        receiver,
                        receiver_class_name,
                        selector_name,
                        regs_before_zombie_farm_record.as_ref().unwrap(),
                    );
                }
                zombie_farm_trace_game_interaction_return(env, receiver, &selector_name_for_after);
                zombie_farm_apply_local_hunger_update(env, receiver, &selector_name_for_after);
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
