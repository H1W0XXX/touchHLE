/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! `UIView`.
//!
//! Useful resources:
//! - Apple's [View Programming Guide for iOS](https://developer.apple.com/library/archive/documentation/WindowsViews/Conceptual/ViewPG_iPhoneOS/Introduction/Introduction.html)

pub mod ui_alert_view;
pub mod ui_control;
pub mod ui_image_view;
pub mod ui_label;
pub mod ui_picker_view;
pub mod ui_scroll_view;
pub mod ui_web_view;
pub mod ui_window;

use super::ui_gesture_recognizer;
use super::ui_graphics::{UIGraphicsPopContext, UIGraphicsPushContext};
use crate::abi::CallFromHost;
use crate::frameworks::core_animation::ca_layer;
use crate::frameworks::core_graphics::cg_affine_transform::{
    CGAffineTransform, CGAffineTransformIdentity,
};
use crate::frameworks::core_graphics::cg_color::CGColorRef;
use crate::frameworks::core_graphics::cg_context::{CGContextClearRect, CGContextRef};
use crate::frameworks::core_graphics::cg_geometry::CGRectZero;
use crate::frameworks::core_graphics::{CGFloat, CGPoint, CGRect, CGSize};
use crate::frameworks::foundation::ns_string::get_static_str;
use crate::frameworks::foundation::{ns_array, NSInteger, NSTimeInterval, NSUInteger};
use crate::mem::{ConstPtr, ConstVoidPtr, MutVoidPtr, SafeRead};
use crate::objc::{
    autorelease, id, msg, msg_class, msg_send, nil, objc_classes, release, retain,
    todo_objc_setter, Class, ClassExports, HostObject, NSZonePtr, ObjC, SEL,
};
use crate::Environment;
use std::collections::HashSet;
use std::io::Write;

pub struct State {
    /// List of views for internal purposes. Non-retaining!
    pub(super) views: Vec<id>,
    pub ui_window: ui_window::State,
    pub animations_enabled: bool,
    animation: Option<UIViewAnimationState>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            views: Vec::new(),
            ui_window: ui_window::State::default(),
            animations_enabled: true,
            animation: None,
        }
    }
}

struct UIViewAnimationState {
    animation_id: id,
    context: MutVoidPtr,
    delegate: id,
    will_start_selector: Option<SEL>,
    did_stop_selector: Option<SEL>,
}

#[repr(C, packed)]
struct BlockLiteral {
    _isa: u32,
    _flags: i32,
    _reserved: i32,
    invoke: crate::abi::GuestFunction,
}
unsafe impl SafeRead for BlockLiteral {}

pub(super) struct UIViewHostObject {
    /// CALayer or subclass.
    layer: id,
    /// Subviews in back-to-front order. These are strong references.
    subviews: Vec<id>,
    /// The superview. This is a weak reference.
    superview: id,
    /// The view controller that controls this view. This is a weak reference
    view_controller: id,
    gesture_recognizers: Vec<id>,
    tag: NSInteger,
    clips_to_bounds: bool,
    clears_context_before_drawing: bool,
    content_mode: NSInteger,
    user_interaction_enabled: bool,
    multiple_touch_enabled: bool,
}
impl HostObject for UIViewHostObject {}
impl Default for UIViewHostObject {
    fn default() -> UIViewHostObject {
        // The Default trait is implemented so subclasses will get the same
        // defaults.
        UIViewHostObject {
            layer: nil,
            subviews: Vec::new(),
            superview: nil,
            view_controller: nil,
            gesture_recognizers: Vec::new(),
            tag: 0,
            clips_to_bounds: false,
            clears_context_before_drawing: true,
            content_mode: 0,
            user_interaction_enabled: true,
            multiple_touch_enabled: false,
        }
    }
}

pub fn get_clips_to_bounds(objc: &ObjC, view: id) -> bool {
    objc.borrow::<UIViewHostObject>(view).clips_to_bounds
}

fn is_zombie_farm(env: &Environment) -> bool {
    env.bundle
        .bundle_identifier()
        .starts_with("com.playforge.Z")
}

fn is_zombie_farm_2(env: &Environment) -> bool {
    env.bundle.bundle_identifier() == "com.playforge.ZombieFarm2"
}

fn zombie_farm_view_layer_bounds_position(
    env: &Environment,
    view: id,
) -> Option<(CGRect, CGPoint)> {
    if view == nil || env.objc.get_host_object(view).is_none() {
        return None;
    }
    let layer = env.objc.borrow::<UIViewHostObject>(view).layer;
    if layer == nil || env.objc.get_host_object(layer).is_none() {
        return None;
    }
    let (_delegate, bounds, position, _anchor, _sublayers) =
        ca_layer::diagnostic_snapshot(&env.objc, layer);
    Some((bounds, position))
}

fn zombie_farm_is_hud_root(env: &Environment, view: id) -> bool {
    if view == nil || env.objc.get_host_object(view).is_none() {
        return false;
    }
    let host = env.objc.borrow::<UIViewHostObject>(view);
    if host.user_interaction_enabled || host.subviews.len() < 20 {
        return false;
    }
    let Some((bounds, position)) = zombie_farm_view_layer_bounds_position(env, view) else {
        return false;
    };
    bounds.origin == (CGPoint { x: 0.0, y: 0.0 })
        && bounds.size.width >= 1024.0
        && bounds.size.height >= 768.0
        && position.x == 512.0
        && position.y == 384.0
}

fn zombie_farm_should_force_hud_visible(env: &Environment, view: id) -> bool {
    if !is_zombie_farm_2(env) || view == nil || env.objc.get_host_object(view).is_none() {
        return false;
    }

    let class_name = debug_class_name(env, view);
    let superview = env.objc.borrow::<UIViewHostObject>(view).superview;
    if !zombie_farm_is_hud_root(env, superview) {
        return false;
    }
    let Some((_bounds, position)) = zombie_farm_view_layer_bounds_position(env, view) else {
        return false;
    };

    let is_hud_widget = class_name.contains("Button")
        || class_name == "UIImageView"
        || class_name == "FarmHUDQuestIcon";
    is_hud_widget && (position.x >= 940.0 || class_name == "FarmHUDQuestIcon")
}

fn zombie_farm_adjust_forced_hud_visible_view(env: &mut Environment, view: id) {
    if debug_class_name(env, view) != "Toolbar" {
        return;
    }
    let Some((_bounds, position)) = zombie_farm_view_layer_bounds_position(env, view) else {
        return;
    };
    if position.x <= 1024.0 {
        return;
    }
    let center = CGPoint {
        x: 796.0,
        y: position.y,
    };
    () = msg![env; view setCenter:center];
}

fn zombie_farm_force_hud_visible_if_needed(env: &mut Environment, view: id) -> bool {
    if !zombie_farm_should_force_hud_visible(env, view) {
        return false;
    }
    zombie_farm_adjust_forced_hud_visible_view(env, view);
    let layer = env.objc.borrow::<UIViewHostObject>(view).layer;
    if layer != nil && env.objc.get_host_object(layer).is_some() {
        () = msg![env; layer setHidden:false];
    }
    true
}

fn zombie_farm_reveal_farm_hud_controls_inner(env: &mut Environment, view: id) -> u32 {
    if view == nil || env.objc.get_host_object(view).is_none() {
        return 0;
    }

    let subviews = env.objc.borrow::<UIViewHostObject>(view).subviews.clone();
    let mut revealed = u32::from(zombie_farm_force_hud_visible_if_needed(env, view));
    for subview in subviews {
        revealed += zombie_farm_reveal_farm_hud_controls_inner(env, subview);
    }
    revealed
}

pub fn reveal_zombie_farm_hud_controls(env: &mut Environment) -> u32 {
    let windows = env.framework_state.uikit.ui_view.ui_window.windows.clone();
    let mut revealed = 0;
    for window in windows {
        revealed += zombie_farm_reveal_farm_hud_controls_inner(env, window);
    }
    revealed
}

fn live_layer_or_nil(env: &mut Environment, view: id, selector_name: &str) -> id {
    let layer = env.objc.borrow::<UIViewHostObject>(view).layer;
    if layer == nil || env.objc.get_host_object(layer).is_some() {
        return layer;
    }

    if is_zombie_farm(env) {
        log!(
            "ZombieFarm workaround: UIView {:?} has released layer {:?} during {}, ignoring layer message",
            view,
            layer,
            selector_name
        );
        nil
    } else {
        layer
    }
}

fn debug_class_name(env: &Environment, object: id) -> String {
    if object == nil {
        return "nil".to_string();
    }
    let class = ObjC::read_isa(object, &env.mem);
    if class == nil {
        return "<nil isa>".to_string();
    }
    env.objc
        .try_get_class_name(class)
        .unwrap_or("<unknown class>")
        .to_string()
}

fn dump_view_tree_inner(
    env: &Environment,
    writer: &mut dyn Write,
    view: id,
    depth: usize,
    visited: &mut HashSet<u32>,
) -> std::io::Result<()> {
    let indent = "  ".repeat(depth);
    if view == nil {
        writeln!(writer, "{indent}<nil view>")?;
        return Ok(());
    }

    if !visited.insert(view.to_bits()) {
        writeln!(
            writer,
            "{indent}0x{:x} {} (cycle)",
            view.to_bits(),
            debug_class_name(env, view)
        )?;
        return Ok(());
    }

    let host = env.objc.borrow::<UIViewHostObject>(view);
    let layer = host.layer;
    let subviews = host.subviews.clone();
    let tag = host.tag;
    let user_interaction_enabled = host.user_interaction_enabled;
    let clips_to_bounds = host.clips_to_bounds;
    let superview = host.superview;

    let layer_summary = if layer != nil && env.objc.get_host_object(layer).is_some() {
        let (_delegate, bounds, position, anchor, _sublayers) =
            ca_layer::diagnostic_snapshot(&env.objc, layer);
        let (
            hidden,
            opaque,
            opacity,
            has_background,
            has_contents,
            has_presented_pixels,
            has_cg_context,
            has_gl_texture,
        ) = ca_layer::diagnostic_render_snapshot(&env.objc, layer);
        format!(
            "layer=0x{:x} bounds={:?} position={} anchor={} hidden={} opaque={} opacity={} bg={} contents={} presented_pixels={} cg_context={} gl_texture={}",
            layer.to_bits(),
            bounds,
            position,
            anchor,
            hidden,
            opaque,
            opacity,
            has_background,
            has_contents,
            has_presented_pixels,
            has_cg_context,
            has_gl_texture,
        )
    } else {
        format!("layer=0x{:x} <missing host object>", layer.to_bits())
    };

    writeln!(
        writer,
        "{indent}0x{:x} {} tag={} super=0x{:x} subviews={} userInteraction={} clips={} {}",
        view.to_bits(),
        debug_class_name(env, view),
        tag,
        superview.to_bits(),
        subviews.len(),
        user_interaction_enabled,
        clips_to_bounds,
        layer_summary
    )?;

    for subview in subviews {
        dump_view_tree_inner(env, writer, subview, depth + 1, visited)?;
    }

    Ok(())
}

pub fn dump_debug_inspector(env: &mut Environment) {
    let path = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join("zombie_farm_inspector.txt");

    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&path)?;
        writeln!(file, "touchHLE inspector dump")?;
        writeln!(file, "bundle: {}", env.bundle.bundle_identifier())?;
        writeln!(file)?;

        crate::zombie_farm_debug::write_snapshot(&mut file)?;
        writeln!(file)?;
        crate::zombie_farm_debug::write_actor_snapshot(env, &mut file)?;
        writeln!(file)?;
        writeln!(file, "== UIKit View Hierarchy ==")?;
        let windows = env.framework_state.uikit.ui_view.ui_window.windows.clone();
        if windows.is_empty() {
            writeln!(file, "(no UIWindow objects)")?;
        }
        let mut visited = HashSet::new();
        for window in windows {
            dump_view_tree_inner(env, &mut file, window, 0, &mut visited)?;
        }
        Ok(())
    })();

    match result {
        Ok(()) => {
            log!("Inspector dump written to {}", path.display());
        }
        Err(err) => {
            log!("Failed to write inspector dump {}: {}", path.display(), err);
        }
    }
}

fn remove_subviews_by_class_inner(env: &mut Environment, view: id, class_name: &str) -> u32 {
    if view == nil || env.objc.get_host_object(view).is_none() {
        return 0;
    }

    let subviews = env.objc.borrow::<UIViewHostObject>(view).subviews.clone();
    let mut removed = 0;
    for subview in subviews {
        if debug_class_name(env, subview) == class_name {
            () = msg![env; subview removeFromSuperview];
            removed += 1;
        } else {
            removed += remove_subviews_by_class_inner(env, subview, class_name);
        }
    }
    removed
}

fn has_subviews(env: &Environment, view: id) -> bool {
    env.objc.get_host_object(view).is_some_and(|_| {
        !env.objc
            .borrow::<UIViewHostObject>(view)
            .subviews
            .is_empty()
    })
}

fn zombie_farm_touch_trace_enabled(env: &Environment) -> bool {
    is_zombie_farm(env) && std::env::var("TOUCHHLE_ZF2_TOUCH_TRACE").ok().as_deref() == Some("1")
}

fn zombie_farm_direct_control_hit_test(env: &mut Environment, view: id, point: CGPoint) -> id {
    let view_layer = env.objc.borrow::<UIViewHostObject>(view).layer;
    let subviews = env.objc.borrow::<UIViewHostObject>(view).subviews.clone();
    for subview in subviews.into_iter().rev() {
        let hidden: bool = msg![env; subview isHidden];
        let alpha: CGFloat = msg![env; subview alpha];
        if hidden || alpha < 0.01 || env.objc.get_host_object(subview).is_none() {
            continue;
        }

        let layer = env.objc.borrow::<UIViewHostObject>(subview).layer;
        if layer == nil || env.objc.get_host_object(layer).is_none() {
            continue;
        }
        let local: CGPoint = msg![env; layer convertPoint:point fromLayer:view_layer];
        let contains: bool = msg![env; layer containsPoint:local];
        if !contains {
            continue;
        }

        let hit = zombie_farm_direct_control_hit_test(env, subview, local);
        if hit != nil {
            return hit;
        }

        let interactible: bool = msg![env; subview isUserInteractionEnabled];
        if !interactible {
            continue;
        }
        let ui_control_class = env.objc.get_known_class("UIControl", &mut env.mem);
        let class: Class = msg![env; subview class];
        let class_name = debug_class_name(env, subview);
        if env.objc.class_is_subclass_of(class, ui_control_class)
            || class_name.contains("Button")
            || class_name == "FarmHUDQuestIcon"
        {
            if is_zombie_farm_2(env) || zombie_farm_touch_trace_enabled(env) {
                log!(
                    "ZombieFarm2 UI hit trace: direct hit {:?} ({}) parent {:?} ({}) point {} local {}",
                    subview,
                    class_name,
                    view,
                    debug_class_name(env, view),
                    point,
                    local,
                );
            }
            return subview;
        }
    }
    nil
}

pub fn remove_subviews_by_class(env: &mut Environment, class_name: &str) -> u32 {
    let windows = env.framework_state.uikit.ui_view.ui_window.windows.clone();
    let mut removed = 0;
    for window in windows {
        removed += remove_subviews_by_class_inner(env, window, class_name);
    }
    removed
}

fn call_animation_selector(
    env: &mut Environment,
    delegate: id,
    selector: Option<SEL>,
    animation_id: id,
    finished: bool,
    context: MutVoidPtr,
) {
    let Some(selector) = selector else {
        return;
    };
    if selector.is_null() {
        return;
    }

    let selector_name = selector.as_str(&env.mem);
    match selector_name.bytes().filter(|&b| b == b':').count() {
        0 => {
            let _: () = msg_send(env, (delegate, selector));
        }
        1 => {
            let _: () = msg_send(env, (delegate, selector, animation_id));
        }
        2 => {
            let _: () = msg_send(env, (delegate, selector, animation_id, finished));
        }
        3 => {
            let _: () = msg_send(env, (delegate, selector, animation_id, finished, context));
        }
        _ => {
            log!(
                "Warning: UIView animation selector {:?} has too many arguments",
                selector_name
            );
        }
    }
}

fn call_animation_block(env: &mut Environment, block: ConstPtr<BlockLiteral>) {
    if block.is_null() {
        return;
    }

    let block_literal: BlockLiteral = env.mem.read(block);
    let invoke = block_literal.invoke;
    if invoke.addr_with_thumb_bit() != 0 {
        let block_ptr: ConstVoidPtr = block.cast();
        () = invoke.call_from_host(env, (block_ptr,));
    }
}

fn call_animation_completion_block(env: &mut Environment, block: ConstPtr<BlockLiteral>) {
    if block.is_null() {
        return;
    }

    let block_literal: BlockLiteral = env.mem.read(block);
    let invoke = block_literal.invoke;
    if invoke.addr_with_thumb_bit() != 0 {
        let block_ptr: ConstVoidPtr = block.cast();
        () = invoke.call_from_host(env, (block_ptr, true));
    }
}

pub fn set_view_controller(env: &mut Environment, view: id, controller: id) {
    let host_obj = env.objc.borrow_mut::<UIViewHostObject>(view);
    host_obj.view_controller = controller;
}

/// Shared parts of `initWithCoder:` and `initWithFrame:`. These can't call
/// `init`: the subclass may have overridden `init` and will not expect to be
/// called here.
///
/// Do not call this in subclasses of `UIView`.
fn init_common(env: &mut Environment, this: id) -> id {
    let view_class: Class = msg![env; this class];
    let layer_class: Class = msg![env; view_class layerClass];
    let layer: id = msg![env; layer_class layer];

    // CALayer is not opaque by default, but UIView is
    () = msg![env; layer setDelegate:this];
    () = msg![env; layer setOpaque:true];

    env.objc.borrow_mut::<UIViewHostObject>(this).layer = layer;

    env.framework_state.uikit.ui_view.views.push(this);

    this
}

pub const CLASSES: ClassExports = objc_classes! {

(env, this, _cmd);

@implementation UIView: UIResponder

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::<UIViewHostObject>::default();
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

+ (Class)layerClass {
    env.objc.get_known_class("CALayer", &mut env.mem)
}

+ (())beginAnimations:(id)animation_id context:(MutVoidPtr)context {
    env.framework_state.uikit.ui_view.animation = Some(UIViewAnimationState {
        animation_id,
        context,
        delegate: nil,
        will_start_selector: None,
        did_stop_selector: None,
    });
}

+ (())commitAnimations {
    let Some(animation) = env.framework_state.uikit.ui_view.animation.take() else {
        return;
    };
    if animation.delegate == nil {
        return;
    }

    call_animation_selector(
        env,
        animation.delegate,
        animation.will_start_selector,
        animation.animation_id,
        true,
        animation.context,
    );
    call_animation_selector(
        env,
        animation.delegate,
        animation.did_stop_selector,
        animation.animation_id,
        true,
        animation.context,
    );
}

+ (())setAnimationDuration:(NSTimeInterval)_duration {
}

+ (())setAnimationDelay:(NSTimeInterval)_delay {
}

+ (())setAnimationCurve:(NSInteger)_curve {
}

+ (())setAnimationDelegate:(id)delegate {
    if let Some(animation) = &mut env.framework_state.uikit.ui_view.animation {
        animation.delegate = delegate;
    }
}

+ (())setAnimationWillStartSelector:(SEL)selector {
    if let Some(animation) = &mut env.framework_state.uikit.ui_view.animation {
        animation.will_start_selector = Some(selector);
    }
}

+ (())setAnimationDidStopSelector:(SEL)selector {
    if let Some(animation) = &mut env.framework_state.uikit.ui_view.animation {
        animation.did_stop_selector = Some(selector);
    }
}

+ (())setAnimationBeginsFromCurrentState:(bool)_from_current_state {
}

+ (())setAnimationRepeatCount:(CGFloat)_repeat_count {
}

+ (())setAnimationRepeatAutoreverses:(bool)_repeat_autoreverses {
}

+ (())setAnimationTransition:(NSInteger)_transition forView:(id)_view cache:(bool)_cache {
}

+ (())setAnimationsEnabled:(bool)enabled {
    env.framework_state.uikit.ui_view.animations_enabled = enabled;
}

+ (bool)areAnimationsEnabled {
    env.framework_state.uikit.ui_view.animations_enabled
}

+ (())animateWithDuration:(NSTimeInterval)_duration
               animations:(ConstPtr<BlockLiteral>)animations {
    call_animation_block(env, animations);
}

+ (())animateWithDuration:(NSTimeInterval)_duration
               animations:(ConstPtr<BlockLiteral>)animations
               completion:(ConstPtr<BlockLiteral>)completion {
    call_animation_block(env, animations);
    call_animation_completion_block(env, completion);
}

+ (())animateWithDuration:(NSTimeInterval)_duration
                    delay:(NSTimeInterval)_delay
                  options:(NSUInteger)_options
               animations:(ConstPtr<BlockLiteral>)animations
               completion:(ConstPtr<BlockLiteral>)completion {
    call_animation_block(env, animations);
    call_animation_completion_block(env, completion);
}

+ (())transitionWithView:(id)_view
                duration:(NSTimeInterval)_duration
                 options:(NSUInteger)_options
              animations:(ConstPtr<BlockLiteral>)animations
              completion:(ConstPtr<BlockLiteral>)completion {
    call_animation_block(env, animations);
    call_animation_completion_block(env, completion);
}

// TODO: accessors etc

// initWithCoder: and initWithFrame: are basically UIView's designated
// initializers. init is not, it's a shortcut for the latter.
// Subclasses need to override both.

- (id)init {
    msg![env; this initWithFrame:(<CGRect as Default>::default())]
}

- (id)initWithFrame:(CGRect)frame {
    let this = init_common(env, this);

    () = msg![env; this setFrame:frame];

    log_dbg!(
        "[(UIView*){:?} initWithFrame:{:?}] => bounds {:?}, center {:?}",
        this,
        frame,
        { let bounds: CGRect = msg![env; this bounds]; bounds },
        { let center: CGPoint = msg![env; this center]; center },
    );

    this
}

// NSCoding implementation
- (id)initWithCoder:(id)coder {
    let this = init_common(env, this);

    // TODO: decode the various other UIView properties

    let key_ns_string = get_static_str(env, "UIBounds");
    let bounds: CGRect = msg![env; coder decodeCGRectForKey:key_ns_string];

    let key_ns_string = get_static_str(env, "UICenter");
    let center: CGPoint = msg![env; coder decodeCGPointForKey:key_ns_string];

    let key_ns_string = get_static_str(env, "UIHidden");
    let hidden: bool = msg![env; coder decodeBoolForKey:key_ns_string];

    let key_ns_string = get_static_str(env, "UIOpaque");
    let opaque: bool = msg![env; coder decodeBoolForKey:key_ns_string];

    let key_ns_string = get_static_str(env, "UIBackgroundColor");
    let bg_color: id = msg![env; coder decodeObjectForKey:key_ns_string];

    let key_ns_string = get_static_str(env, "UITag");
    let tag: NSInteger = msg![env; coder decodeIntegerForKey:key_ns_string];

    let key_ns_string = get_static_str(env, "UIMultipleTouchEnabled");
    let multi_touch_enabled: bool = msg![env; coder decodeBoolForKey:key_ns_string];

    let key_ns_string = get_static_str(env, "UISubviews");
    let subviews: id = msg![env; coder decodeObjectForKey:key_ns_string];
    let subview_count: NSUInteger = msg![env; subviews count];

    log_dbg!(
        "[(UIView*){:?} initWithCoder:{:?}] => bounds {}, center {}, hidden {}, bg color {:?}, tag {}, opaque {}, multi touch enabled {}, {} subviews",
        this,
        coder,
        bounds,
        center,
        hidden,
        bg_color,
        tag,
        opaque,
        multi_touch_enabled,
        subview_count,
    );

    () = msg![env; this setBounds:bounds];
    () = msg![env; this setCenter:center];
    () = msg![env; this setHidden:hidden];
    () = msg![env; this setOpaque:opaque];
    () = msg![env; this setBackgroundColor:bg_color];
    () = msg![env; this setTag:tag];
    () = msg![env; this setMultipleTouchEnabled:multi_touch_enabled];

    for i in 0..subview_count {
        let subview: id = msg![env; subviews objectAtIndex:i];
        () = msg![env; this addSubview:subview];
    }

    this
}

- (NSInteger)tag {
    env.objc.borrow::<UIViewHostObject>(this).tag
}
- (())setTag:(NSInteger)tag {
    env.objc.borrow_mut::<UIViewHostObject>(this).tag = tag;
}

- (id)viewWithTag:(NSInteger)tag {
    let (view_tag, subviews) = {
        let host = env.objc.borrow::<UIViewHostObject>(this);
        (host.tag, host.subviews.clone())
    };
    if view_tag == tag {
        return this;
    }
    for subview in subviews {
        let found: id = msg![env; subview viewWithTag:tag];
        if found != nil {
            return found;
        }
    }
    nil
}

- (bool)isUserInteractionEnabled {
    env.objc.borrow::<UIViewHostObject>(this).user_interaction_enabled
}
- (())setUserInteractionEnabled:(bool)enabled {
    env.objc.borrow_mut::<UIViewHostObject>(this).user_interaction_enabled = enabled;
}

- (bool)isMultipleTouchEnabled {
    env.objc.borrow::<UIViewHostObject>(this).multiple_touch_enabled
}
- (())setMultipleTouchEnabled:(bool)enabled {
    env.objc.borrow_mut::<UIViewHostObject>(this).multiple_touch_enabled = enabled;
}

- (())setExclusiveTouch:(bool)exclusive {
    log_dbg!("Ignoring setExclusiveTouch:{} for view {:?}", exclusive, this);
}

- (())layoutSubviews {
    // On iOS 5.1 and earlier, the default implementation of this method does
    // nothing.
}
- (())setNeedsLayout {
    // TODO: defer this until the next layout pass.
    () = msg![env; this layoutSubviews];
}
- (())layoutIfNeeded {
    () = msg![env; this layoutSubviews];
}

- (id)superview {
    env.objc.borrow::<UIViewHostObject>(this).superview
}

- (id)window {
    // Looks up window in the superview hierarchy
    // TODO: cache the result somehow?
    let mut window: id = env.objc.borrow::<UIViewHostObject>(this).superview;
    let window_class = env.objc.get_known_class("UIWindow", &mut env.mem);
    while window != nil {
        let current_class: Class = msg![env; window class];
        log_dbg!("maybe window {:?} curr class {}", window, env.objc.get_class_name(current_class));
        if env.objc.class_is_subclass_of(current_class, window_class) {
            break;
        }
        window = env.objc.borrow::<UIViewHostObject>(window).superview;
    }
    log_dbg!("view {:?} has window {:?}", this, window);
    window
}

- (id)subviews {
    let views = env.objc.borrow::<UIViewHostObject>(this).subviews.clone();
    for view in &views {
        retain(env, *view);
    }
    let subs = ns_array::from_vec(env, views);
    autorelease(env, subs)
}

- (id)gestureRecognizers {
    let recognizers = env.objc.borrow::<UIViewHostObject>(this).gesture_recognizers.clone();
    for recognizer in &recognizers {
        retain(env, *recognizer);
    }
    let array = ns_array::from_vec(env, recognizers);
    autorelease(env, array)
}

- (())setGestureRecognizers:(id)recognizers {
    let old_recognizers = std::mem::take(
        &mut env.objc.borrow_mut::<UIViewHostObject>(this).gesture_recognizers
    );
    for recognizer in old_recognizers {
        ui_gesture_recognizer::set_view(env, recognizer, nil);
        release(env, recognizer);
    }

    if recognizers == nil {
        return;
    }

    let count: NSUInteger = msg![env; recognizers count];
    for idx in 0..count {
        let recognizer: id = msg![env; recognizers objectAtIndex:idx];
        () = msg![env; this addGestureRecognizer:recognizer];
    }
}

- (())addGestureRecognizer:(id)recognizer {
    if recognizer == nil {
        return;
    }

    {
        let gesture_recognizers =
            &mut env.objc.borrow_mut::<UIViewHostObject>(this).gesture_recognizers;
        if gesture_recognizers.contains(&recognizer) {
            return;
        }
    }

    retain(env, recognizer);
    env.objc
        .borrow_mut::<UIViewHostObject>(this)
        .gesture_recognizers
        .push(recognizer);
    ui_gesture_recognizer::set_view(env, recognizer, this);
}

- (())removeGestureRecognizer:(id)recognizer {
    if recognizer == nil {
        return;
    }

    let gesture_recognizers = &mut env.objc.borrow_mut::<UIViewHostObject>(this).gesture_recognizers;
    let Some(idx) = gesture_recognizers.iter().position(|&item| item == recognizer) else {
        return;
    };

    let recognizer = gesture_recognizers.remove(idx);
    ui_gesture_recognizer::set_view(env, recognizer, nil);
    release(env, recognizer);
}

- (())addSubview:(id)view {
    log_dbg!("[(UIView*){:?} addSubview:{:?}] => ()", this, view);

    if view == nil {
        log_dbg!("Tolerating [(UIView*){:?} addSubview:nil]", this);
        return;
    }

    if env.objc.borrow::<UIViewHostObject>(view).superview == this {
        () = msg![env; this bringSubviewToFront:view];
    } else {
        retain(env, view);
        () = msg![env; view removeFromSuperview];
        let subview_obj = env.objc.borrow_mut::<UIViewHostObject>(view);
        subview_obj.superview = this;
        let subview_layer = subview_obj.layer;
        let this_obj = env.objc.borrow_mut::<UIViewHostObject>(this);
        this_obj.subviews.push(view);
        let this_layer = this_obj.layer;
        () = msg![env; this_layer addSublayer:subview_layer];
    }
}

- (())insertSubview:(id)view atIndex:(NSInteger)index {
    assert!(view != nil);
    retain(env, view);
    () = msg![env; view removeFromSuperview];

    let subview_obj = env.objc.borrow_mut::<UIViewHostObject>(view);
    subview_obj.superview = this;
    let subview_layer = subview_obj.layer;

    let &mut UIViewHostObject {
        ref mut subviews,
        layer: this_layer,
        ..
    } = env.objc.borrow_mut(this);

    subviews.insert(index as usize, view);

    assert!(index >= 0);
    () = msg![env; this_layer insertSublayer:subview_layer atIndex:(index as u32)];
}

- (())insertSubview:(id)view belowSubview:(id)sibling {
    retain(env, view);
    () = msg![env; view removeFromSuperview];

    let subview_obj = env.objc.borrow_mut::<UIViewHostObject>(view);
    subview_obj.superview = this;
    let subview_layer = subview_obj.layer;

    let sibling_layer = env.objc.borrow_mut::<UIViewHostObject>(sibling).layer;

    let &mut UIViewHostObject {
        ref mut subviews,
        layer: this_layer,
        ..
    } = env.objc.borrow_mut(this);

    let idx = subviews.iter().position(|&subview2| subview2 == sibling).unwrap();
    subviews.insert(idx, view);

    () = msg![env; this_layer insertSublayer:subview_layer below:sibling_layer];
}

- (())insertSubview:(id)view aboveSubview:(id)sibling {
    retain(env, view);
    () = msg![env; view removeFromSuperview];

    let subview_obj = env.objc.borrow_mut::<UIViewHostObject>(view);
    subview_obj.superview = this;
    let subview_layer = subview_obj.layer;

    let &mut UIViewHostObject {
        ref mut subviews,
        layer: this_layer,
        ..
    } = env.objc.borrow_mut(this);

    let idx = subviews.iter().position(|&subview2| subview2 == sibling).unwrap();
    subviews.insert(idx + 1, view);

    () = msg![env; this_layer insertSublayer:subview_layer atIndex:((idx + 1) as u32)];
}

- (())bringSubviewToFront:(id)subview {
    if subview == nil {
        // This happens in Touch & Go LITE. It's probably due to the ad classes
        // being replaced with fakes.
        log_dbg!("Tolerating [{:?} bringSubviewToFront:nil]", this);
        return;
    }

    let &mut UIViewHostObject {
        ref mut subviews,
        layer,
        ..
    } = env.objc.borrow_mut(this);

    let Some(idx) = subviews.iter().position(|&subview2| subview2 == subview) else {
        log_dbg!("Warning: Unable to find the subview {:?} in subviews of {:?}", subview, this);
        return;
    };
    let subview2 = subviews.remove(idx);
    assert!(subview2 == subview);
    subviews.push(subview);

    let subview_layer = env.objc.borrow::<UIViewHostObject>(subview).layer;
    () = msg![env; subview_layer removeFromSuperlayer];
    () = msg![env; layer addSublayer:subview_layer];
}

- (())sendSubviewToBack:(id)subview {
    if subview == nil {
        log_dbg!("Tolerating [{:?} sendSubviewToBack:nil]", this);
        return;
    }

    let &mut UIViewHostObject {
        ref mut subviews,
        layer,
        ..
    } = env.objc.borrow_mut(this);

    let Some(idx) = subviews.iter().position(|&subview2| subview2 == subview) else {
        log_dbg!("Warning: Unable to find the subview {:?} in subviews of {:?}", subview, this);
        return;
    };
    let subview2 = subviews.remove(idx);
    assert!(subview2 == subview);
    subviews.insert(0, subview);

    let subview_layer = env.objc.borrow::<UIViewHostObject>(subview).layer;
    () = msg![env; subview_layer removeFromSuperlayer];
    () = msg![env; layer insertSublayer:subview_layer atIndex:0u32];
}

- (())removeFromSuperview {
    log_dbg!(
        "ZombieFarm trace: UIView {:?} removeFromSuperview",
        this,
    );
    let &mut UIViewHostObject {
        ref mut superview,
        layer: this_layer,
        ..
    } = env.objc.borrow_mut(this);
    let superview = std::mem::take(superview);
    if superview == nil {
        return;
    }
    () = msg![env; this_layer removeFromSuperlayer];

    let UIViewHostObject { ref mut subviews, .. } = env.objc.borrow_mut(superview);
    let idx = subviews.iter().position(|&subview| subview == this).unwrap();
    let subview = subviews.remove(idx);
    assert!(subview == this);
    release(env, this);
}

- (())dealloc {
    let UIViewHostObject {
        layer,
        superview,
        subviews,
        view_controller,
        gesture_recognizers,
        tag: _,
        clips_to_bounds: _,
        clears_context_before_drawing: _,
        content_mode: _,
        user_interaction_enabled: _,
        multiple_touch_enabled: _,
    } = std::mem::take(env.objc.borrow_mut(this));

    release(env, layer);
    assert!(view_controller == nil);
    assert!(superview == nil);
    for subview in subviews {
        env.objc.borrow_mut::<UIViewHostObject>(subview).superview = nil;
        release(env, subview);
    }
    for recognizer in gesture_recognizers {
        ui_gesture_recognizer::set_view(env, recognizer, nil);
        release(env, recognizer);
    }

    let state = &mut env.framework_state.uikit.ui_view.views;
    state.swap_remove(
        state.iter().position(|&v| v == this).unwrap()
    );

    env.objc.dealloc_object(this, &mut env.mem);
}

- (id)layer {
    env.objc.borrow_mut::<UIViewHostObject>(this).layer
}

- (bool)isHidden {
    let layer = live_layer_or_nil(env, this, "isHidden");
    if layer == nil {
        return false;
    }
    msg![env; layer isHidden]
}
- (())setHidden:(bool)hidden {
    log_dbg!(
        "ZombieFarm trace: UIView {:?} setHidden:{}",
        this,
        hidden,
    );
    let layer = live_layer_or_nil(env, this, "setHidden:");
    if layer == nil {
        return;
    }
    msg![env; layer setHidden:hidden]
}

- (bool)clipsToBounds {
    env.objc.borrow::<UIViewHostObject>(this).clips_to_bounds
}
- (())setClipsToBounds:(bool)clips {
    env.objc.borrow_mut::<UIViewHostObject>(this).clips_to_bounds = clips;
    if env.bundle.bundle_identifier().starts_with("com.playforge.Z") {
        log_dbg!(
            "ZombieFarm trace: UIView {:?} setClipsToBounds:{} frame:{:?} bounds:{:?}",
            this,
            clips,
            {
                let layer = live_layer_or_nil(env, this, "setClipsToBounds: frame");
                if layer == nil {
                    CGRectZero
                } else {
                    let frame: CGRect = msg![env; layer frame];
                    frame
                }
            },
            {
                let layer = live_layer_or_nil(env, this, "setClipsToBounds: bounds");
                if layer == nil {
                    CGRectZero
                } else {
                    let bounds: CGRect = msg![env; layer bounds];
                    bounds
                }
            },
        );
    }
}

- (bool)isOpaque {
    let layer = live_layer_or_nil(env, this, "isOpaque");
    if layer == nil {
        return false;
    }
    msg![env; layer isOpaque]
}
- (())setOpaque:(bool)opaque {
    let layer = live_layer_or_nil(env, this, "setOpaque:");
    if layer == nil {
        return;
    }
    msg![env; layer setOpaque:opaque]
}

- (CGFloat)alpha {
    let layer = live_layer_or_nil(env, this, "alpha");
    if layer == nil {
        return 1.0;
    }
    msg![env; layer opacity]
}
- (())setAlpha:(CGFloat)alpha {
    log_dbg!(
        "ZombieFarm trace: UIView {:?} setAlpha:{}",
        this,
        alpha,
    );
    let layer = live_layer_or_nil(env, this, "setAlpha:");
    if layer == nil {
        return;
    }
    msg![env; layer setOpacity:alpha]
}

- (id)backgroundColor {
    let layer = live_layer_or_nil(env, this, "backgroundColor");
    if layer == nil {
        return nil;
    }
    let cg_color: CGColorRef = msg![env; layer backgroundColor];
    msg_class![env; UIColor colorWithCGColor:cg_color]
}
- (())setBackgroundColor:(id)color { // UIColor*
    let color: CGColorRef = msg![env; color CGColor];
    let layer = live_layer_or_nil(env, this, "setBackgroundColor:");
    if layer == nil {
        return;
    }
    msg![env; layer setBackgroundColor:color]
}

// TODO: support setNeedsDisplayInRect:
- (())setNeedsDisplay {
    // UIView has a method called drawRect: that subclasses override if they
    // need custom drawing. touchHLE's UIView (a CALayerDelegate) provides
    // an implementation of drawLayer:inContext: that calls drawRect:.
    // This maintains a clean separation of UIView and CALayer.
    //
    // To avoid wasting space and time on unnecessary bitmaps and drawing,
    // let's optimize here by only marking the layer as needing display if
    // the UIView's subclass overrides drawRect: or drawLayer:inContext:.
    let this_class = ObjC::read_isa(this, &env.mem);

    let ui_view_class = env.objc.get_known_class("UIView", &mut env.mem);

    let draw_layer_sel = env.objc.lookup_selector("drawLayer:inContext:").unwrap();
    let draw_rect_sel = env.objc.lookup_selector("drawRect:").unwrap();

    if env
        .objc
        .class_overrides_method_of_superclass(this_class, draw_rect_sel, ui_view_class)
        || env
            .objc
            .class_overrides_method_of_superclass(this_class, draw_layer_sel, ui_view_class)
    {
        let layer = live_layer_or_nil(env, this, "setNeedsDisplay");
        if layer == nil {
            return;
        }
        msg![env; layer setNeedsDisplay]
    }
}

- (CGRect)bounds {
    let layer = live_layer_or_nil(env, this, "bounds");
    if layer == nil {
        return CGRectZero;
    }
    msg![env; layer bounds]
}
- (())setBounds:(CGRect)bounds {
    log_dbg!(
        "ZombieFarm trace: UIView {:?} setBounds:{:?}",
        this,
        bounds,
    );
    let layer = live_layer_or_nil(env, this, "setBounds:");
    if layer == nil {
        return;
    }
    () = msg![env; layer setBounds:bounds];
    () = msg![env; this layoutSubviews];
}
- (CGPoint)center {
    // FIXME: what happens if [layer anchorPoint] isn't (0.5, 0.5)?
    let layer = live_layer_or_nil(env, this, "center");
    if layer == nil {
        return CGPoint { x: 0.0, y: 0.0 };
    }
    msg![env; layer position]
}
- (())setCenter:(CGPoint)center {
    log_dbg!(
        "ZombieFarm trace: UIView {:?} setCenter:{:?}",
        this,
        center,
    );
    let layer = live_layer_or_nil(env, this, "setCenter:");
    if layer == nil {
        return;
    }
    msg![env; layer setPosition:center]
}
- (CGRect)frame {
    let layer = live_layer_or_nil(env, this, "frame");
    if layer == nil {
        return CGRectZero;
    }
    msg![env; layer frame]
}
- (())setFrame:(CGRect)frame {
    log_dbg!(
        "ZombieFarm trace: UIView {:?} setFrame:{:?}",
        this,
        frame,
    );
    let layer = live_layer_or_nil(env, this, "setFrame:");
    if layer == nil {
        return;
    }
    () = msg![env; layer setFrame:frame];
    () = msg![env; this layoutSubviews];
}
- (CGAffineTransform)transform {
    let layer = live_layer_or_nil(env, this, "transform");
    if layer == nil {
        return CGAffineTransformIdentity;
    }
    msg![env; layer affineTransform]
}
- (())setTransform:(CGAffineTransform)transform {
    log_dbg!(
        "ZombieFarm trace: UIView {:?} setTransform:{:?}",
        this,
        transform,
    );
    let layer = live_layer_or_nil(env, this, "setTransform:");
    if layer == nil {
        return;
    }
    msg![env; layer setAffineTransform:transform]
}

- (())setContentMode:(NSInteger)content_mode { // should be UIViewContentMode
    env.objc.borrow_mut::<UIViewHostObject>(this).content_mode = content_mode;
}
- (NSInteger)contentMode {
    env.objc.borrow::<UIViewHostObject>(this).content_mode
}

- (bool)clearsContextBeforeDrawing {
    env.objc.borrow::<UIViewHostObject>(this).clears_context_before_drawing
}
- (())setClearsContextBeforeDrawing:(bool)v {
    env.objc.borrow_mut::<UIViewHostObject>(this).clears_context_before_drawing = v;
}

// Drawing stuff that views should override
- (())drawRect:(CGRect)_rect {
    // default implementation does nothing
}

// CALayerDelegate implementation
- (())drawLayer:(id)layer // CALayer*
      inContext:(CGContextRef)context {
    let mut bounds: CGRect = msg![env; layer bounds];
    bounds.origin = CGPoint { x: 0.0, y: 0.0 }; // FIXME: not tested
    if env.objc.borrow::<UIViewHostObject>(this).clears_context_before_drawing {
        CGContextClearRect(env, context, bounds);
    }
    UIGraphicsPushContext(env, context);
    () = msg![env; this drawRect:bounds];
    UIGraphicsPopContext(env);
}

// Event handling

- (bool)pointInside:(CGPoint)point
          withEvent:(id)_event { // UIEvent* (possibly nil)
    let layer = env.objc.borrow::<UIViewHostObject>(this).layer;
    msg![env; layer containsPoint:point]
}

- (id)hitTest:(CGPoint)point
    withEvent:(id)event { // UIEvent* (possibly nil)
    if !msg![env; this pointInside:point withEvent:event] {
        return nil;
    }
    if is_zombie_farm(env) && debug_class_name(env, this) == "EAGLView" {
        let trace_enabled = zombie_farm_touch_trace_enabled(env);
        if trace_enabled {
            log!("ZombieFarm2 UI hit trace: EAGLView hitTest point {}", point);
        }
        let mut hit = zombie_farm_direct_control_hit_test(env, this, point);
        if hit == nil {
            let window: id = msg![env; this window];
            if window != nil {
                let window_bounds: CGRect = msg![env; window bounds];
                let landscape_point = CGPoint {
                    x: point.x,
                    y: window_bounds.size.height - point.y,
                };
                if trace_enabled {
                    log!(
                        "ZombieFarm2 UI hit trace: trying y-flipped point {} with window bounds {}",
                        landscape_point,
                        window_bounds,
                    );
                }
                hit = zombie_farm_direct_control_hit_test(env, this, landscape_point);
            }
        }
        if hit != nil {
            return hit;
        }
    }
    // TODO: avoid copy somehow?
    let subviews = env.objc.borrow::<UIViewHostObject>(this).subviews.clone();
    for subview in subviews.into_iter().rev() { // later views are on top
        let hidden: bool = msg![env; subview isHidden];
        let alpha: CGFloat = msg![env; subview alpha];
        let interactible: bool = msg![env; subview isUserInteractionEnabled];
        let zombie_farm_disabled_container =
            is_zombie_farm(env) && !interactible && has_subviews(env, subview);
        if hidden || alpha < 0.01 || (!interactible && !zombie_farm_disabled_container) {
           continue;
        }
        let point: CGPoint = msg![env; subview convertPoint:point fromView:this];
        let subview: id = msg![env; subview hitTest:point withEvent:event];
        if subview != nil {
            return subview;
        }
    }
    if is_zombie_farm(env) {
        let interactible: bool = msg![env; this isUserInteractionEnabled];
        if !interactible {
            return nil;
        }
    }
    if is_zombie_farm(env) && debug_class_name(env, this) == "WhiteDimLayer" {
        log!("ZombieFarm workaround: ignoring stale WhiteDimLayer {:?} during hit testing", this);
        return nil;
    }
    if is_zombie_farm(env) && debug_class_name(env, this) == "PermeableView" {
        log_dbg!(
            "ZombieFarm workaround: letting PermeableView {:?} pass through hit testing",
            this,
        );
        return nil;
    }
    this
}

// Ending a view-editing session

- (bool)endEditing:(bool)force {
    assert!(force);
    let responder: id = env.framework_state.uikit.ui_responder.first_responder;
    let class = msg![env; responder class];
    let ui_text_field_class = env.objc.get_known_class("UITextField", &mut env.mem);
    if responder != nil && env.objc.class_is_subclass_of(class, ui_text_field_class) {
        // we need to check if text field is in the current view hierarchy
        let mut to_find = responder;
        while to_find != nil {
            if to_find == this {
                return msg![env; responder resignFirstResponder];
            }
            to_find = msg![env; to_find superview];
        }
    }
    false
}

// UIResponder implementation
// From the Apple UIView docs regarding [UIResponder nextResponder]:
// "UIView implements this method and returns the UIViewController object that
//  manages it (if it has one) or its superview (if it doesn’t)."
- (id)nextResponder {
    let host_object = env.objc.borrow::<UIViewHostObject>(this);
    if host_object.view_controller != nil {
        host_object.view_controller
    } else {
        host_object.superview
    }
}

// Co-ordinate space conversion

- (CGPoint)convertPoint:(CGPoint)point
               fromView:(id)other { // UIView*
    if other == nil {
        let window: id = msg![env; this window];
        assert!(window != nil);
        return msg![env; this convertPoint:point fromView:window]
    }
    let this_layer = env.objc.borrow::<UIViewHostObject>(this).layer;
    let other_layer = env.objc.borrow::<UIViewHostObject>(other).layer;
    msg![env; this_layer convertPoint:point fromLayer:other_layer]
}
- (CGPoint)convertPoint:(CGPoint)point
                 toView:(id)other { // UIView*
    if other == nil {
        let window: id = msg![env; this window];
        assert!(window != nil);
        return msg![env; this convertPoint:point toView:window]
    }
    let this_layer = env.objc.borrow::<UIViewHostObject>(this).layer;
    let other_layer = env.objc.borrow::<UIViewHostObject>(other).layer;
    msg![env; this_layer convertPoint:point toLayer:other_layer]
}
- (CGRect)convertRect:(CGRect)rect
             fromView:(id)other { // UIView*
    if other == nil {
        let window: id = msg![env; this window];
        assert!(window != nil);
        return msg![env; this convertRect:rect fromView:window]
    }
    let this_layer = env.objc.borrow::<UIViewHostObject>(this).layer;
    let other_layer = env.objc.borrow::<UIViewHostObject>(other).layer;
    msg![env; this_layer convertRect:rect fromLayer:other_layer]
}
- (CGRect)convertRect:(CGRect)rect
               toView:(id)other { // UIView*
    if other == nil {
        let window: id = msg![env; this window];
        assert!(window != nil);
        return msg![env; this convertRect:rect toView:window]
    }
    let this_layer = env.objc.borrow::<UIViewHostObject>(this).layer;
    let other_layer = env.objc.borrow::<UIViewHostObject>(other).layer;
    msg![env; this_layer convertRect:rect toLayer:other_layer]
}

- (())setAutoresizingMask:(NSUInteger)mask {
    log_dbg!("Ignoring setAutoresizingMask:{} for view {:?}", mask, this);
}
- (())setAutoresizesSubviews:(bool)enabled {
    log_dbg!("Ignoring setAutoresizesSubviews:{} for view {:?}", enabled, this);
}

- (CGSize)sizeThatFits:(CGSize)size {
    // default implementation, subclasses can override
    size
}
- (())sizeToFit {
    log!("TODO: [(UIView *){:?} sizeToFit]", this);
}

- (())setContentScaleFactor:(CGFloat)factor {
    todo_objc_setter!(this, factor);
}
- (CGFloat)contentScaleFactor {
    1.0 // TODO
}

@end

};
