use crate::frameworks::core_foundation::time::SECS_FROM_UNIX_TO_APPLE_EPOCHS;
use crate::frameworks::core_graphics::{CGPoint, CGRect, CGSize};
use crate::frameworks::foundation::{ns_date, ns_string, NSUInteger};
use crate::fs::GuestPath;
use crate::mem::ConstVoidPtr;
use crate::objc::{id, msg_send_no_type_checking, nil, ObjC};
use crate::Environment;
use std::collections::{BTreeMap, VecDeque};
use std::io::{Result as IoResult, Write};
use std::sync::{Mutex, OnceLock};

const RECENT_LIMIT: usize = 160;
const HUNGER_RECENT_LIMIT: usize = 2048;

#[derive(Default)]
struct TableState {
    class_name: String,
    direction: Option<u32>,
    view_size: Option<CGSize>,
    content_size: Option<CGSize>,
    content_offset: Option<CGPoint>,
    last_index_from_offset: Option<u32>,
    last_offset_from_index_arg: Option<u32>,
    last_set_index_arg: Option<u32>,
    last_set_index_cell: Option<u32>,
    recent: VecDeque<String>,
}

#[derive(Default)]
struct State {
    tables: BTreeMap<u32, TableState>,
    recent: VecDeque<String>,
    hunger_events: VecDeque<String>,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

fn state() -> &'static Mutex<State> {
    STATE.get_or_init(|| Mutex::new(State::default()))
}

pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED
        .get_or_init(|| std::env::var("TOUCHHLE_ZOMBIE_FARM_DEBUG").ok().as_deref() == Some("1"))
}

fn push_recent(recent: &mut VecDeque<String>, line: String) {
    if recent.len() >= RECENT_LIMIT {
        recent.pop_front();
    }
    recent.push_back(line);
}

fn push_recent_hunger(recent: &mut VecDeque<String>, line: String) {
    if recent.len() >= HUNGER_RECENT_LIMIT {
        recent.pop_front();
    }
    recent.push_back(line);
}

pub fn record_table_size_return(receiver: id, class_name: &str, selector_name: &str, size: CGSize) {
    if receiver.is_null() || !(class_name.contains("TableView") || class_name == "CCScrollView") {
        return;
    }

    let receiver_bits = receiver.to_bits();
    let mut state = state().lock().unwrap();
    let table = state.tables.entry(receiver_bits).or_default();
    table.class_name = class_name.to_string();
    match selector_name {
        "viewSize" => table.view_size = Some(size),
        "contentSize" => table.content_size = Some(size),
        _ => {}
    }

    let line = format!("[0x{receiver_bits:x} {class_name} {selector_name}] return size={size}");
    push_recent(&mut table.recent, line.clone());
    push_recent(&mut state.recent, line);
}

pub fn record_table_point_return(
    receiver: id,
    class_name: &str,
    selector_name: &str,
    point: CGPoint,
) {
    if receiver.is_null() || !(class_name.contains("TableView") || class_name == "CCScrollView") {
        return;
    }

    let receiver_bits = receiver.to_bits();
    let mut state = state().lock().unwrap();
    let table = state.tables.entry(receiver_bits).or_default();
    table.class_name = class_name.to_string();
    if selector_name == "contentOffset" {
        table.content_offset = Some(point);
    }

    let line = format!("[0x{receiver_bits:x} {class_name} {selector_name}] return point={point}");
    push_recent(&mut table.recent, line.clone());
    push_recent(&mut state.recent, line);
}

pub fn record_table_value_return(receiver: id, class_name: &str, selector_name: &str, value: u32) {
    if receiver.is_null() || !(class_name.contains("TableView") || class_name == "CCScrollView") {
        return;
    }

    let receiver_bits = receiver.to_bits();
    let mut state = state().lock().unwrap();
    let table = state.tables.entry(receiver_bits).or_default();
    table.class_name = class_name.to_string();
    match selector_name {
        "direction" => table.direction = Some(value),
        "_indexFromOffset:" => table.last_index_from_offset = Some(value),
        _ => {}
    }

    let line = format!("[0x{receiver_bits:x} {class_name} {selector_name}] return value={value}");
    push_recent(&mut table.recent, line.clone());
    push_recent(&mut state.recent, line);
}

pub fn record_table_args(receiver: id, class_name: &str, selector_name: &str, regs: &[u32; 16]) {
    if receiver.is_null() || !(class_name.contains("TableView") || class_name == "CCScrollView") {
        return;
    }

    let receiver_bits = receiver.to_bits();
    let mut state = state().lock().unwrap();
    let table = state.tables.entry(receiver_bits).or_default();
    table.class_name = class_name.to_string();

    let detail = match selector_name {
        "setContentSize:" => {
            let size = CGSize {
                width: f32::from_bits(regs[2]),
                height: f32::from_bits(regs[3]),
            };
            table.content_size = Some(size);
            format!("arg size={size}")
        }
        "setViewSize:" => {
            let size = CGSize {
                width: f32::from_bits(regs[2]),
                height: f32::from_bits(regs[3]),
            };
            table.view_size = Some(size);
            format!("arg size={size}")
        }
        "setContentOffset:" => {
            let point = CGPoint {
                x: f32::from_bits(regs[2]),
                y: f32::from_bits(regs[3]),
            };
            table.content_offset = Some(point);
            format!("arg point={point}")
        }
        "setDirection:" => {
            table.direction = Some(regs[2]);
            format!("arg value={}", regs[2])
        }
        "_offsetFromIndex:" => {
            // _offsetFromIndex: returns a CGPoint and is therefore normally
            // reached through objc_msgSend_stret. In that ABI r0 is the return
            // pointer, r1/r2 are receiver/selector, and the first real method
            // argument is in r3.
            table.last_offset_from_index_arg = Some(regs[3]);
            format!("arg index={}", regs[3])
        }
        "_setIndex:forCell:" => {
            table.last_set_index_arg = Some(regs[2]);
            table.last_set_index_cell = Some(regs[3]);
            format!("arg index={} cell=0x{:x}", regs[2], regs[3])
        }
        "_addCellIfNecessary:" | "_moveCellOutOfSight:" => {
            format!("arg cell=0x{:x}", regs[2])
        }
        "dequeueCell" | "_evictCell" => "(no args)".to_string(),
        "cellWithIndex:" => format!("arg index={}", regs[2]),
        "_indexFromOffset:" => {
            let point = CGPoint {
                x: f32::from_bits(regs[2]),
                y: f32::from_bits(regs[3]),
            };
            format!("arg point={point}")
        }
        "scrollViewDidScroll:" => format!("arg object=0x{:x}", regs[2]),
        _ => return,
    };

    let line = format!("[0x{receiver_bits:x} {class_name} {selector_name}] {detail}");
    push_recent(&mut table.recent, line.clone());
    push_recent(&mut state.recent, line);
}

pub fn record_layout_event(line: String) {
    let mut state = state().lock().unwrap();
    push_recent(&mut state.recent, line);
}

pub fn record_objc_message(receiver: id, class_name: &str, selector_name: &str, regs: &[u32; 16]) {
    let line = format!(
        "[objc call] [0x{:x} {} {}] r2=0x{:x} r3=0x{:x}",
        receiver.to_bits(),
        class_name,
        selector_name,
        regs[2],
        regs[3],
    );

    let mut state = state().lock().unwrap();
    push_recent(&mut state.recent, line);
}

pub fn should_record_hunger_message(class_name: &str, selector_name: &str) -> bool {
    let is_zombie_actor = class_name.starts_with("ZombieActor");
    let is_zombie_menu = class_name == "ZFZombieMenu";
    let is_game_state = matches!(class_name, "ZFGuiLayer" | "GameState" | "GameData");

    (is_zombie_actor
        && matches!(
            selector_name,
            "hunger" | "hungerLevel" | "setHunger:" | "setEatDate:"
        ))
        || (is_zombie_menu
            && matches!(
                selector_name,
                "displayHunger" | "updateSelectedZombieInfo" | "currentZombie"
            ))
        || (is_game_state
            && matches!(
                selector_name,
                "applyZombieHunger"
                    | "fixZombieHunger"
                    | "saveDate"
                    | "setSaveDate:"
                    | "getServerTime"
                    | "handleTimeResponse:"
                    | "handleResponse:forAction:"
                    | "statusCheckDone"
                    | "startUpChecksComplete"
            ))
}

fn string_object_to_debug(env: &mut Environment, string: id) -> String {
    if string == nil {
        return "nil".to_string();
    }

    let Some(class) = debug_object_class(env, string) else {
        return format!("0x{:x} <invalid object>", string.to_bits());
    };

    let class_name = env
        .objc
        .try_get_class_name(class)
        .unwrap_or("<unknown class>")
        .to_string();
    let string_class = env.objc.get_known_class("NSString", &mut env.mem);
    if env.objc.class_is_subclass_of(class, string_class) {
        let value = ns_string::to_rust_string(env, string);
        return format!("0x{:x} {class_name} {:?}", string.to_bits(), value);
    }

    format!("0x{:x} {class_name}", string.to_bits())
}

fn object_to_debug(env: &Environment, object: id) -> String {
    if object == nil {
        return "nil".to_string();
    }

    if debug_object_class(env, object).is_none() {
        return format!("0x{:x} <invalid object>", object.to_bits());
    }

    format!("0x{:x} {}", object.to_bits(), debug_class_name(env, object))
}

fn date_to_debug(env: &Environment, date: id) -> String {
    let object = object_to_debug(env, date);
    let Some(interval) = ns_date::debug_time_interval(env, date) else {
        return object;
    };
    let unix = interval + SECS_FROM_UNIX_TO_APPLE_EPOCHS as f64;
    format!("{object} ref={interval:.3} unix={unix:.3}")
}

pub fn record_hunger_message(
    env: &Environment,
    receiver: id,
    class_name: &str,
    selector_name: &str,
    regs: &[u32; 16],
) {
    if !should_record_hunger_message(class_name, selector_name) {
        return;
    }

    let detail = match selector_name {
        "setHunger:" => format!("arg hunger={:.3}", f32::from_bits(regs[2])),
        "setEatDate:" | "setSaveDate:" => {
            format!("arg date={}", date_to_debug(env, id::from_bits(regs[2])))
        }
        "handleTimeResponse:" => {
            format!(
                "arg object={}",
                object_to_debug(env, id::from_bits(regs[2]))
            )
        }
        "handleResponse:forAction:" => format!(
            "arg response={} action={}",
            object_to_debug(env, id::from_bits(regs[2])),
            object_to_debug(env, id::from_bits(regs[3]))
        ),
        "setString:" => format!(
            "arg string={}",
            object_to_debug(env, id::from_bits(regs[2]))
        ),
        _ => String::new(),
    };

    let suffix = if detail.is_empty() {
        String::new()
    } else {
        format!(" {detail}")
    };
    let line = format!(
        "[call] [{} {}] receiver={}{}",
        class_name,
        selector_name,
        object_to_debug(env, receiver),
        suffix
    );

    let mut state = state().lock().unwrap();
    push_recent_hunger(&mut state.hunger_events, line);
}

pub fn record_hunger_return(
    env: &Environment,
    receiver: id,
    class_name: &str,
    selector_name: &str,
) {
    if !should_record_hunger_message(class_name, selector_name) {
        return;
    }

    let detail = match selector_name {
        "hunger" => format!("return hunger={:.3}", f32::from_bits(env.cpu.regs()[0])),
        "hungerLevel" => format!("return hungerLevel={}", env.cpu.regs()[0]),
        "eatDate" | "currentZombie" | "saveDate" => {
            format!(
                "return object={}",
                date_to_debug(env, id::from_bits(env.cpu.regs()[0]))
            )
        }
        _ => String::new(),
    };

    let suffix = if detail.is_empty() {
        String::new()
    } else {
        format!(" {detail}")
    };
    let line = format!(
        "[return] [{} {}] receiver={}{}",
        class_name,
        selector_name,
        object_to_debug(env, receiver),
        suffix
    );

    let mut state = state().lock().unwrap();
    push_recent_hunger(&mut state.hunger_events, line);
}

pub fn should_record_apply_trace_message(class_name: &str, selector_name: &str) -> bool {
    let interesting_class = class_name.starts_with("ZombieActor")
        || matches!(
            class_name,
            "NSDate"
                | "ActiveProfileStatus"
                | "GameState"
                | "GameData"
                | "PlayerProfile"
                | "ZFActorManager"
                | "ZFGuiLayer"
                | "_touchHLE_NSArray"
                | "_touchHLE_NSMutableArray"
        );
    let interesting_selector = matches!(
        selector_name,
        "actorList"
            | "addTimeInterval:"
            | "applyZombieHunger"
            | "count"
            | "date"
            | "eatDate"
            | "gameState"
            | "getActivePlayer"
            | "getBeginningOfTheDayFromDate:"
            | "handleTimeResponse:"
            | "hunger"
            | "isEqual:"
            | "isValid"
            | "latestStatus"
            | "objectAtIndex:"
            | "saveDate"
            | "serverDate"
            | "setEatDate:"
            | "setHunger:"
            | "setSaveDate:"
            | "timeIntervalSinceDate:"
            | "timeIntervalSinceNow"
            | "timeIntervalSinceReferenceDate"
            | "zfGameData"
    );
    interesting_class && interesting_selector
}

fn f64_arg_from_regs(regs: &[u32; 16], start: usize) -> f64 {
    let mut bytes = [0u8; 8];
    bytes[0..4].copy_from_slice(&regs[start].to_le_bytes());
    bytes[4..8].copy_from_slice(&regs[start + 1].to_le_bytes());
    f64::from_bits(u64::from_le_bytes(bytes))
}

fn f64_return_from_regs(regs: &[u32; 16]) -> f64 {
    f64_arg_from_regs(regs, 0)
}

pub fn record_apply_trace_message(
    env: &Environment,
    receiver: id,
    class_name: &str,
    selector_name: &str,
    regs: &[u32; 16],
) {
    if !should_record_apply_trace_message(class_name, selector_name) {
        return;
    }

    let detail = match selector_name {
        "addTimeInterval:" => format!(" arg seconds={:.3}", f64_arg_from_regs(regs, 2)),
        "getBeginningOfTheDayFromDate:"
        | "isEqual:"
        | "setEatDate:"
        | "setSaveDate:"
        | "timeIntervalSinceDate:" => {
            format!(" arg object={}", date_to_debug(env, id::from_bits(regs[2])))
        }
        "objectAtIndex:" => format!(" arg index={}", regs[2]),
        "setHunger:" => format!(" arg hunger={:.3}", f32::from_bits(regs[2])),
        _ => String::new(),
    };

    let line = format!(
        "[apply call] [{} {}] receiver={}{}",
        class_name,
        selector_name,
        object_to_debug(env, receiver),
        detail
    );
    let mut state = state().lock().unwrap();
    push_recent_hunger(&mut state.hunger_events, line);
}

pub fn record_apply_trace_return(
    env: &Environment,
    receiver: id,
    class_name: &str,
    selector_name: &str,
) {
    if !should_record_apply_trace_message(class_name, selector_name) {
        return;
    }

    let detail = match selector_name {
        "count" => format!(" return count={}", env.cpu.regs()[0]),
        "hunger" => format!(" return hunger={:.3}", f32::from_bits(env.cpu.regs()[0])),
        "isEqual:" => format!(" return bool={}", env.cpu.regs()[0]),
        "timeIntervalSinceDate:" | "timeIntervalSinceNow" | "timeIntervalSinceReferenceDate" => {
            format!(
                " return seconds={:.3}",
                f64_return_from_regs(env.cpu.regs())
            )
        }
        "addTimeInterval:"
        | "date"
        | "eatDate"
        | "getBeginningOfTheDayFromDate:"
        | "saveDate"
        | "serverDate" => {
            format!(
                " return date={}",
                date_to_debug(env, id::from_bits(env.cpu.regs()[0]))
            )
        }
        "actorList" | "gameState" | "objectAtIndex:" | "zfGameData" => {
            format!(
                " return object={}",
                object_to_debug(env, id::from_bits(env.cpu.regs()[0]))
            )
        }
        _ => String::new(),
    };

    let line = format!(
        "[apply return] [{} {}] receiver={}{}",
        class_name,
        selector_name,
        object_to_debug(env, receiver),
        detail
    );
    let mut state = state().lock().unwrap();
    push_recent_hunger(&mut state.hunger_events, line);
}

pub fn record_table_object_return(
    receiver: id,
    class_name: &str,
    selector_name: &str,
    object: id,
    object_class_name: Option<&str>,
) {
    let object_bits = object.to_bits();
    let object_desc = match object_class_name {
        Some(name) => format!("0x{object_bits:x} {name}"),
        None if object.is_null() => "nil".to_string(),
        None => format!("0x{object_bits:x} <unknown class>"),
    };

    let line = format!(
        "[0x{:x} {class_name} {selector_name}] return object={object_desc}",
        receiver.to_bits()
    );

    let mut state = state().lock().unwrap();
    if !receiver.is_null() && (class_name.contains("TableView") || class_name == "CCScrollView") {
        let table = state.tables.entry(receiver.to_bits()).or_default();
        table.class_name = class_name.to_string();
        push_recent(&mut table.recent, line.clone());
    }
    push_recent(&mut state.recent, line);
}

pub fn write_snapshot(mut writer: impl Write) -> IoResult<()> {
    let state = state().lock().unwrap();

    writeln!(writer, "== Zombie Farm CCTableView Inspector ==")?;
    if state.tables.is_empty() {
        writeln!(writer, "(no CCTableView messages recorded yet)")?;
    }
    for (receiver, table) in &state.tables {
        writeln!(writer)?;
        writeln!(writer, "Table 0x{receiver:x} {}", table.class_name)?;
        writeln!(writer, "  direction: {:?}", table.direction)?;
        writeln!(writer, "  view_size: {:?}", table.view_size)?;
        writeln!(writer, "  content_size: {:?}", table.content_size)?;
        writeln!(writer, "  content_offset: {:?}", table.content_offset)?;
        writeln!(
            writer,
            "  last_index_from_offset: {:?}",
            table.last_index_from_offset
        )?;
        writeln!(
            writer,
            "  last_offset_from_index_arg: {:?}",
            table.last_offset_from_index_arg
        )?;
        writeln!(
            writer,
            "  last_set_index: {:?}, cell: {:?}",
            table.last_set_index_arg.map(|v| format!("{v}")),
            table.last_set_index_cell.map(|v| format!("0x{v:x}"))
        )?;
        writeln!(writer, "  recent:")?;
        for line in &table.recent {
            writeln!(writer, "    {line}")?;
        }
    }

    writeln!(writer)?;
    writeln!(writer, "== Recent Layout Events ==")?;
    for line in &state.recent {
        writeln!(writer, "{line}")?;
    }

    writeln!(writer)?;
    writeln!(writer, "== Recent Hunger Events ==")?;
    for line in &state.hunger_events {
        writeln!(writer, "{line}")?;
    }

    Ok(())
}

fn debug_class_name(env: &Environment, object: id) -> String {
    if object == nil {
        return "nil".to_string();
    }
    let Some(class) = debug_object_class(env, object) else {
        return "<invalid object>".to_string();
    };
    env.objc
        .try_get_class_name(class)
        .unwrap_or("<unknown class>")
        .to_string()
}

fn debug_object_class(env: &Environment, object: id) -> Option<id> {
    let bits = object.to_bits();
    if object == nil || bits < env.mem.null_segment_size() || bits % 4 != 0 {
        return None;
    }
    if env
        .mem
        .get_bytes_fallible(ConstVoidPtr::from_bits(bits), 4)
        .is_none()
    {
        return None;
    }

    let class = ObjC::read_isa(object, &env.mem);
    if class == nil || class.to_bits() % 4 != 0 {
        return None;
    }
    env.objc.get_host_object(class)?;
    Some(class)
}

fn debug_object_has_method(env: &Environment, object: id, selector: crate::objc::SEL) -> bool {
    debug_object_class(env, object).is_some()
        && env.objc.object_has_method(&env.mem, object, selector)
}

fn read_object_ivar(env: &Environment, object: id, name: &str) -> Option<id> {
    debug_object_class(env, object)?;
    let ivar = env
        .objc
        .object_lookup_ivar(&env.mem, object, &name.to_string())?;
    Some(env.mem.read(ivar.cast()))
}

fn read_f32_ivar(env: &Environment, object: id, name: &str) -> Option<f32> {
    debug_object_class(env, object)?;
    let ivar = env
        .objc
        .object_lookup_ivar(&env.mem, object, &name.to_string())?;
    Some(env.mem.read(ivar.cast()))
}

fn get_game_state(env: &mut Environment) -> Option<id> {
    let game_state_class = env.objc.get_known_class("GameState", &mut env.mem);
    let game_state_selector = env.objc.lookup_selector("gameState")?;
    if !env
        .objc
        .object_has_method(&env.mem, game_state_class, game_state_selector)
    {
        return None;
    }
    let game_state: id = msg_send_no_type_checking(env, (game_state_class, game_state_selector));
    if game_state == nil {
        return None;
    }
    Some(game_state)
}

fn get_game_data(env: &mut Environment) -> Option<id> {
    let game_state = get_game_state(env)?;

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
    Some(game_data)
}

fn get_gui_layer(env: &mut Environment) -> Option<id> {
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

fn get_running_scene(env: &mut Environment) -> Option<id> {
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

fn get_actor_list_from_game_state(env: &mut Environment) -> Option<id> {
    let game_data = get_game_data(env)?;
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

fn count_if_collection(env: &mut Environment, object: id) -> Option<NSUInteger> {
    let count_selector = env.objc.lookup_selector("count")?;
    if !debug_object_has_method(env, object, count_selector) {
        return None;
    }
    Some(msg_send_no_type_checking(env, (object, count_selector)))
}

fn object_at_index_if_collection(
    env: &mut Environment,
    object: id,
    index: NSUInteger,
) -> Option<id> {
    let object_at_index_selector = env.objc.lookup_selector("objectAtIndex:")?;
    if !debug_object_has_method(env, object, object_at_index_selector) {
        return None;
    }
    Some(msg_send_no_type_checking(
        env,
        (object, object_at_index_selector, index),
    ))
}

fn object_to_debug_with_count(env: &mut Environment, object: id) -> String {
    let mut description = object_to_debug(env, object);
    if let Some(count) = count_if_collection(env, object) {
        description.push_str(&format!(" count={count}"));
    }
    description
}

fn node_point_by_getter(env: &mut Environment, object: id, selector_name: &str) -> Option<CGPoint> {
    let selector = env.objc.lookup_selector(selector_name)?;
    debug_object_has_method(env, object, selector)
        .then(|| msg_send_no_type_checking(env, (object, selector)))
}

fn node_size_by_getter(env: &mut Environment, object: id, selector_name: &str) -> Option<CGSize> {
    let selector = env.objc.lookup_selector(selector_name)?;
    debug_object_has_method(env, object, selector)
        .then(|| msg_send_no_type_checking(env, (object, selector)))
}

fn node_f32_by_getter(env: &mut Environment, object: id, selector_name: &str) -> Option<f32> {
    let selector = env.objc.lookup_selector(selector_name)?;
    debug_object_has_method(env, object, selector)
        .then(|| msg_send_no_type_checking(env, (object, selector)))
}

fn node_bool_by_getter(env: &mut Environment, object: id, selector_name: &str) -> Option<bool> {
    let selector = env.objc.lookup_selector(selector_name)?;
    debug_object_has_method(env, object, selector)
        .then(|| msg_send_no_type_checking(env, (object, selector)))
}

fn node_world_origin(env: &mut Environment, object: id) -> Option<CGPoint> {
    let selector = env.objc.lookup_selector("convertToWorldSpace:")?;
    if !debug_object_has_method(env, object, selector) {
        return None;
    }
    let origin = CGPoint { x: 0.0, y: 0.0 };
    Some(msg_send_no_type_checking(env, (object, selector, origin)))
}

fn cocos_node_summary(env: &mut Environment, node: id) -> String {
    let position = node_point_by_getter(env, node, "position")
        .map(|point| point.to_string())
        .unwrap_or_else(|| "n/a".to_string());
    let world = node_world_origin(env, node)
        .map(|point| point.to_string())
        .unwrap_or_else(|| "n/a".to_string());
    let anchor = node_point_by_getter(env, node, "anchorPoint")
        .map(|point| point.to_string())
        .unwrap_or_else(|| "n/a".to_string());
    let size = node_size_by_getter(env, node, "contentSize")
        .map(|size| size.to_string())
        .unwrap_or_else(|| "n/a".to_string());
    let scale_x = node_f32_by_getter(env, node, "scaleX")
        .map(|scale| format!("{scale:.3}"))
        .unwrap_or_else(|| "n/a".to_string());
    let scale_y = node_f32_by_getter(env, node, "scaleY")
        .map(|scale| format!("{scale:.3}"))
        .unwrap_or_else(|| "n/a".to_string());
    let rotation = node_f32_by_getter(env, node, "rotation")
        .map(|rotation| format!("{rotation:.3}"))
        .unwrap_or_else(|| "n/a".to_string());
    let visible = node_bool_by_getter(env, node, "isVisible")
        .or_else(|| node_bool_by_getter(env, node, "visible"))
        .map(|visible| visible.to_string())
        .unwrap_or_else(|| "n/a".to_string());

    format!(
        "pos={position} world0={world} anchor={anchor} size={size} scale=({scale_x},{scale_y}) rot={rotation} visible={visible}"
    )
}

fn dump_optional_ivar(
    env: &mut Environment,
    writer: &mut dyn Write,
    object: id,
    ivar_name: &str,
    depth: usize,
) -> IoResult<()> {
    let indent = "  ".repeat(depth);
    let Some(value) = read_object_ivar(env, object, ivar_name) else {
        return Ok(());
    };
    writeln!(
        writer,
        "{indent}.{ivar_name} = {}",
        object_to_debug_with_count(env, value)
    )
}

fn dump_optional_string_ivar(
    env: &mut Environment,
    writer: &mut dyn Write,
    object: id,
    ivar_name: &str,
    depth: usize,
) -> IoResult<()> {
    let indent = "  ".repeat(depth);
    let Some(value) = read_object_ivar(env, object, ivar_name) else {
        return Ok(());
    };
    writeln!(
        writer,
        "{indent}.{ivar_name} = {}",
        string_object_to_debug(env, value)
    )
}

fn dump_optional_point_ivar(
    env: &Environment,
    writer: &mut dyn Write,
    object: id,
    ivar_name: &str,
    depth: usize,
) -> IoResult<()> {
    let indent = "  ".repeat(depth);
    if debug_object_class(env, object).is_none() {
        return Ok(());
    }
    let Some(ivar) = env
        .objc
        .object_lookup_ivar(&env.mem, object, &ivar_name.to_string())
    else {
        return Ok(());
    };
    let value: CGPoint = env.mem.read(ivar.cast());
    writeln!(writer, "{indent}.{ivar_name} = {value}")
}

fn dump_optional_size_ivar(
    env: &Environment,
    writer: &mut dyn Write,
    object: id,
    ivar_name: &str,
    depth: usize,
) -> IoResult<()> {
    let indent = "  ".repeat(depth);
    if debug_object_class(env, object).is_none() {
        return Ok(());
    }
    let Some(ivar) = env
        .objc
        .object_lookup_ivar(&env.mem, object, &ivar_name.to_string())
    else {
        return Ok(());
    };
    let value: CGSize = env.mem.read(ivar.cast());
    writeln!(writer, "{indent}.{ivar_name} = {value}")
}

fn dump_optional_rect_ivar(
    env: &Environment,
    writer: &mut dyn Write,
    object: id,
    ivar_name: &str,
    depth: usize,
) -> IoResult<()> {
    let indent = "  ".repeat(depth);
    if debug_object_class(env, object).is_none() {
        return Ok(());
    }
    let Some(ivar) = env
        .objc
        .object_lookup_ivar(&env.mem, object, &ivar_name.to_string())
    else {
        return Ok(());
    };
    let value: CGRect = env.mem.read(ivar.cast());
    writeln!(
        writer,
        "{indent}.{ivar_name} = {{{}, {}}}",
        value.origin, value.size
    )
}

fn dump_optional_f32_ivar(
    env: &Environment,
    writer: &mut dyn Write,
    object: id,
    ivar_name: &str,
    depth: usize,
) -> IoResult<()> {
    let indent = "  ".repeat(depth);
    let Some(value) = read_f32_ivar(env, object, ivar_name) else {
        return Ok(());
    };
    writeln!(writer, "{indent}.{ivar_name} = {value:.3}")
}

fn dump_optional_i32_ivar(
    env: &Environment,
    writer: &mut dyn Write,
    object: id,
    ivar_name: &str,
    depth: usize,
) -> IoResult<()> {
    let indent = "  ".repeat(depth);
    if debug_object_class(env, object).is_none() {
        return Ok(());
    }
    let Some(ivar) = env
        .objc
        .object_lookup_ivar(&env.mem, object, &ivar_name.to_string())
    else {
        return Ok(());
    };
    let value: i32 = env.mem.read(ivar.cast());
    writeln!(writer, "{indent}.{ivar_name} = {value}")
}

fn dump_attachment_array_ivar(
    env: &mut Environment,
    writer: &mut dyn Write,
    object: id,
    ivar_name: &str,
    depth: usize,
) -> IoResult<()> {
    let indent = "  ".repeat(depth);
    let Some(array) = read_object_ivar(env, object, ivar_name) else {
        return Ok(());
    };
    let Some(count) = count_if_collection(env, array) else {
        return Ok(());
    };
    if count == 0 {
        return Ok(());
    }
    if depth >= 8 {
        writeln!(
            writer,
            "{indent}.{ivar_name} entries omitted at depth {depth} count={count}"
        )?;
        return Ok(());
    }

    writeln!(
        writer,
        "{indent}.{ivar_name} entries showing {}/{}:",
        count.min(32),
        count
    )?;
    for idx in 0..count.min(32) {
        let entry_indent = "  ".repeat(depth + 1);
        let Some(entry) = object_at_index_if_collection(env, array, idx) else {
            continue;
        };
        writeln!(
            writer,
            "{entry_indent}[{idx}] {}",
            object_to_debug_with_count(env, entry)
        )?;
        dump_texture_debug_ivars(env, writer, entry, depth + 2)?;
    }

    Ok(())
}

fn dump_texture_debug_ivars(
    env: &mut Environment,
    writer: &mut dyn Write,
    node: id,
    depth: usize,
) -> IoResult<()> {
    for ivar_name in [
        "spriteMan",
        "attachments",
        "particles",
        "unitDictionary",
        "actorDictionary",
        "spriteDictionary",
        "skeletonDictionary",
        "fightData",
        "farmData",
        "currentAttackVariation",
        "sprite",
        "actor",
        "childAttachments",
        "parentAttachment",
    ] {
        dump_optional_ivar(env, writer, node, ivar_name, depth)?;
    }
    for ivar_name in ["spriteFileName", "spriteFrameFile", "actionString", "image"] {
        dump_optional_string_ivar(env, writer, node, ivar_name, depth)?;
    }
    for ivar_name in [
        "originalAnchor",
        "offsetFromRefPoint",
        "lastFramePosition",
        "frameOffset",
        "destinationPoint",
        "currentTile",
        "destinationTile",
        "myHomeTile",
        "rootTile",
        "actorSpecificOffset",
        "collisionBoxOffset",
        "knockBackPoint",
        "throwOffset",
        "lifeBarOffset",
    ] {
        dump_optional_point_ivar(env, writer, node, ivar_name, depth)?;
    }
    for ivar_name in ["collisionBoxSize"] {
        dump_optional_size_ivar(env, writer, node, ivar_name, depth)?;
    }
    for ivar_name in ["atlasRect", "rect"] {
        dump_optional_rect_ivar(env, writer, node, ivar_name, depth)?;
    }
    for ivar_name in [
        "lastFrameRotation",
        "changeInRotation",
        "lastFrameScale",
        "originalRotation",
        "rotation",
        "scale",
        "scaleX",
        "scaleY",
        "walkingSpeed",
        "animSpeed",
        "hitPoints",
        "hitPointsTotal",
    ] {
        dump_optional_f32_ivar(env, writer, node, ivar_name, depth)?;
    }
    for ivar_name in [
        "attachmentID",
        "tagID",
        "attachmentZOrder",
        "currentTileX",
        "currentTileY",
        "destinationTileX",
        "destinationTileY",
        "myHomeTileX",
        "myHomeTileY",
        "type",
        "subType",
        "flags",
    ] {
        dump_optional_i32_ivar(env, writer, node, ivar_name, depth)?;
    }
    for ivar_name in ["attachments", "childAttachments"] {
        dump_attachment_array_ivar(env, writer, node, ivar_name, depth)?;
    }
    Ok(())
}

fn dump_cocos_node(
    env: &mut Environment,
    writer: &mut dyn Write,
    node: id,
    depth: usize,
    visited: &mut Vec<u32>,
) -> IoResult<()> {
    if node == nil {
        return Ok(());
    }
    if debug_object_class(env, node).is_none() {
        return Ok(());
    }
    let node_bits = node.to_bits();
    let indent = "  ".repeat(depth);
    if visited.contains(&node_bits) {
        writeln!(writer, "{indent}0x{node_bits:x} <cycle>")?;
        return Ok(());
    }
    visited.push(node_bits);

    let children_selector = env.objc.lookup_selector("children");
    let children = children_selector.and_then(|selector| {
        env.objc
            .object_has_method(&env.mem, node, selector)
            .then(|| msg_send_no_type_checking(env, (node, selector)))
    });
    let child_count = children.and_then(|children| count_if_collection(env, children));
    let node_summary = cocos_node_summary(env, node);
    writeln!(
        writer,
        "{indent}0x{node_bits:x} {} children={} {node_summary}",
        debug_class_name(env, node),
        child_count
            .map(|count| count.to_string())
            .unwrap_or_else(|| "n/a".to_string())
    )?;

    dump_texture_debug_ivars(env, writer, node, depth + 1)?;

    if depth >= 10 {
        return Ok(());
    }
    let Some(children) = children else {
        return Ok(());
    };
    let Some(count) = child_count else {
        return Ok(());
    };
    let Some(object_at_index_selector) = env.objc.lookup_selector("objectAtIndex:") else {
        return Ok(());
    };
    if !env
        .objc
        .object_has_method(&env.mem, children, object_at_index_selector)
    {
        return Ok(());
    }
    for idx in 0..count.min(80) {
        let child: id = msg_send_no_type_checking(env, (children, object_at_index_selector, idx));
        dump_cocos_node(env, writer, child, depth + 1, visited)?;
    }
    Ok(())
}

fn dump_cocos_scene(env: &mut Environment, writer: &mut dyn Write) -> IoResult<()> {
    writeln!(writer, "== Cocos Scene Inspector ==")?;
    let Some(scene) = get_running_scene(env) else {
        writeln!(writer, "(no running scene)")?;
        return Ok(());
    };
    let mut visited = Vec::new();
    dump_cocos_node(env, writer, scene, 0, &mut visited)
}

fn get_actor_list_from_actor_manager(env: &mut Environment) -> Option<id> {
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

fn date_property_by_getter(env: &mut Environment, object: id, selector_name: &str) -> Option<id> {
    let selector = env.objc.lookup_selector(selector_name)?;
    (object != nil && env.objc.object_has_method(&env.mem, object, selector))
        .then(|| msg_send_no_type_checking(env, (object, selector)))
}

fn dump_game_time_state(env: &mut Environment, writer: &mut dyn Write) -> IoResult<()> {
    writeln!(writer, "Time state:")?;

    let date_class = env.objc.get_known_class("NSDate", &mut env.mem);
    let now = env.objc.lookup_selector("date").and_then(|selector| {
        env.objc
            .object_has_method(&env.mem, date_class, selector)
            .then(|| msg_send_no_type_checking(env, (date_class, selector)))
    });
    writeln!(
        writer,
        "  NSDate date: {}",
        now.map(|date| date_to_debug(env, date))
            .unwrap_or_else(|| "n/a".to_string())
    )?;

    let game_data = get_game_data(env);
    let game_state = get_game_state(env);
    let gui_layer = get_gui_layer(env);
    writeln!(
        writer,
        "  TOUCHHLE_FAKE_UNIX_TIME: {}",
        std::env::var("TOUCHHLE_FAKE_UNIX_TIME").unwrap_or_else(|_| "unset".to_string())
    )?;
    writeln!(
        writer,
        "  TOUCHHLE_TIME_OFFSET_SECONDS: {}",
        std::env::var("TOUCHHLE_TIME_OFFSET_SECONDS").unwrap_or_else(|_| "unset".to_string())
    )?;
    writeln!(
        writer,
        "  GameState: {}",
        game_state
            .map(|object| object_to_debug(env, object))
            .unwrap_or_else(|| "n/a".to_string())
    )?;
    if let Some(game_state) = game_state {
        let getter_save_date = date_property_by_getter(env, game_state, "saveDate");
        let ivar_save_date = read_object_ivar(env, game_state, "saveDate");
        writeln!(
            writer,
            "  GameState.saveDate(getter): {}",
            getter_save_date
                .map(|date| date_to_debug(env, date))
                .unwrap_or_else(|| "n/a".to_string())
        )?;
        writeln!(
            writer,
            "  GameState.saveDate(ivar): {}",
            ivar_save_date
                .map(|date| date_to_debug(env, date))
                .unwrap_or_else(|| "n/a".to_string())
        )?;
    }
    writeln!(
        writer,
        "  ZFGuiLayer.gui: {}",
        gui_layer
            .map(|object| object_to_debug(env, object))
            .unwrap_or_else(|| "n/a".to_string())
    )?;
    if let Some(gui_layer) = gui_layer {
        let server_date = read_object_ivar(env, gui_layer, "serverDate");
        writeln!(
            writer,
            "  ZFGuiLayer.serverDate(ivar): {}",
            server_date
                .map(|date| date_to_debug(env, date))
                .unwrap_or_else(|| "n/a".to_string())
        )?;
    }
    writeln!(
        writer,
        "  GameData: {}",
        game_data
            .map(|object| object_to_debug(env, object))
            .unwrap_or_else(|| "n/a".to_string())
    )?;
    if let Some(game_data) = game_data {
        let getter_save_date = date_property_by_getter(env, game_data, "saveDate");
        let ivar_save_date = read_object_ivar(env, game_data, "saveDate");
        writeln!(
            writer,
            "  GameData.saveDate(getter): {}",
            getter_save_date
                .map(|date| date_to_debug(env, date))
                .unwrap_or_else(|| "n/a".to_string())
        )?;
        writeln!(
            writer,
            "  GameData.saveDate(ivar): {}",
            ivar_save_date
                .map(|date| date_to_debug(env, date))
                .unwrap_or_else(|| "n/a".to_string())
        )?;
    }

    let save_path = env.fs.home_directory().join("Documents/saveGame.bin2");
    let save_size = env
        .fs
        .size(GuestPath::new(save_path.as_str()))
        .map(|size| size.to_string())
        .unwrap_or_else(|_| "missing".to_string());
    writeln!(writer, "  saveGame.bin2 size: {save_size}")?;

    Ok(())
}

fn dump_actor_list(
    env: &mut Environment,
    writer: &mut dyn Write,
    title: &str,
    actor_list: Option<id>,
) -> IoResult<()> {
    writeln!(writer, "{title}")?;
    let Some(actor_list) = actor_list else {
        writeln!(writer, "  (not available)")?;
        return Ok(());
    };

    let Some(count_selector) = env.objc.lookup_selector("count") else {
        writeln!(writer, "  (NSArray count selector missing)")?;
        return Ok(());
    };
    let Some(object_at_index_selector) = env.objc.lookup_selector("objectAtIndex:") else {
        writeln!(writer, "  (NSArray objectAtIndex: selector missing)")?;
        return Ok(());
    };
    if !env
        .objc
        .object_has_method(&env.mem, actor_list, count_selector)
        || !env
            .objc
            .object_has_method(&env.mem, actor_list, object_at_index_selector)
    {
        writeln!(
            writer,
            "  0x{:x} {} is not NSArray-like",
            actor_list.to_bits(),
            debug_class_name(env, actor_list)
        )?;
        return Ok(());
    }

    let hunger_selector = env.objc.lookup_selector("hunger");
    let hunger_level_selector = env.objc.lookup_selector("hungerLevel");
    let eat_date_selector = env.objc.lookup_selector("eatDate");
    let time_interval_since_date_selector = env.objc.lookup_selector("timeIntervalSinceDate:");
    let date_class = env.objc.get_known_class("NSDate", &mut env.mem);
    let date_selector = env.objc.lookup_selector("date");
    let now = if let Some(date_selector) = date_selector {
        if env
            .objc
            .object_has_method(&env.mem, date_class, date_selector)
        {
            msg_send_no_type_checking(env, (date_class, date_selector))
        } else {
            nil
        }
    } else {
        nil
    };

    let count: NSUInteger = msg_send_no_type_checking(env, (actor_list, count_selector));
    writeln!(
        writer,
        "  list=0x{:x} {} count={count}",
        actor_list.to_bits(),
        debug_class_name(env, actor_list)
    )?;

    for idx in 0..count {
        let actor: id = msg_send_no_type_checking(env, (actor_list, object_at_index_selector, idx));
        let class_name = debug_class_name(env, actor);
        let getter_hunger = hunger_selector.and_then(|selector| {
            (actor != nil && env.objc.object_has_method(&env.mem, actor, selector))
                .then(|| msg_send_no_type_checking::<f32, _>(env, (actor, selector)))
        });
        let ivar_hunger = (actor != nil)
            .then(|| read_f32_ivar(env, actor, "hunger"))
            .flatten();
        let hunger_level = hunger_level_selector.and_then(|selector| {
            (actor != nil && env.objc.object_has_method(&env.mem, actor, selector))
                .then(|| msg_send_no_type_checking::<u32, _>(env, (actor, selector)))
        });
        let getter_eat_date = eat_date_selector.and_then(|selector| {
            (actor != nil && env.objc.object_has_method(&env.mem, actor, selector))
                .then(|| msg_send_no_type_checking::<id, _>(env, (actor, selector)))
        });
        let ivar_eat_date = (actor != nil)
            .then(|| read_object_ivar(env, actor, "eatDate"))
            .flatten();
        let eat_date = getter_eat_date.or(ivar_eat_date);
        let elapsed = match (now, eat_date, time_interval_since_date_selector) {
            (now, Some(eat_date), Some(selector))
                if now != nil
                    && eat_date != nil
                    && env.objc.object_has_method(&env.mem, now, selector) =>
            {
                Some(msg_send_no_type_checking::<f64, _>(
                    env,
                    (now, selector, eat_date),
                ))
            }
            _ => None,
        };

        writeln!(
            writer,
            "  [{idx:02}] 0x{:x} {:<32} hunger(getter)={} hunger(ivar)={} hungerLevel={} eatDate(getter)={} eatDate(ivar)={} elapsed={}s",
            actor.to_bits(),
            class_name,
            getter_hunger
                .map(|value| format!("{value:.3}"))
                .unwrap_or_else(|| "n/a".to_string()),
            ivar_hunger
                .map(|value| format!("{value:.3}"))
                .unwrap_or_else(|| "n/a".to_string()),
            hunger_level
                .map(|value| format!("{value} (0x{value:x}, f32={:.3})", f32::from_bits(value)))
                .unwrap_or_else(|| "n/a".to_string()),
            getter_eat_date
                .map(|value| date_to_debug(env, value))
                .unwrap_or_else(|| "n/a".to_string()),
            ivar_eat_date
                .map(|value| date_to_debug(env, value))
                .unwrap_or_else(|| "n/a".to_string()),
            elapsed
                .map(|value| format!("{value:.0}"))
                .unwrap_or_else(|| "n/a".to_string()),
        )?;
    }

    Ok(())
}

fn get_label_string(env: &mut Environment, label: id) -> String {
    if label == nil {
        return "nil".to_string();
    }

    for selector_name in ["string", "text"] {
        if let Some(selector) = env.objc.lookup_selector(selector_name) {
            if env.objc.object_has_method(&env.mem, label, selector) {
                let value: id = msg_send_no_type_checking(env, (label, selector));
                return format!("{selector_name}={}", string_object_to_debug(env, value));
            }
        }
    }

    if let Some(value) = read_object_ivar(env, label, "string") {
        return format!("ivar string={}", string_object_to_debug(env, value));
    }
    if let Some(value) = read_object_ivar(env, label, "label") {
        return format!("ivar label={}", object_to_debug(env, value));
    }

    "(no string/text found)".to_string()
}

fn dump_actor_summary(env: &mut Environment, writer: &mut dyn Write, actor: id) -> IoResult<()> {
    writeln!(writer, "  actor={}", object_to_debug(env, actor))?;
    if actor == nil {
        return Ok(());
    }

    if let Some(hunger) = read_f32_ivar(env, actor, "hunger") {
        writeln!(writer, "    ivar hunger={hunger:.3}")?;
    } else {
        writeln!(writer, "    ivar hunger=n/a")?;
    }

    if let Some(selector) = env.objc.lookup_selector("hunger") {
        if env.objc.object_has_method(&env.mem, actor, selector) {
            let hunger: f32 = msg_send_no_type_checking(env, (actor, selector));
            writeln!(writer, "    getter hunger={hunger:.3}")?;
        } else {
            writeln!(writer, "    getter hunger=n/a")?;
        }
    }
    if let Some(selector) = env.objc.lookup_selector("hungerLevel") {
        if env.objc.object_has_method(&env.mem, actor, selector) {
            let value: u32 = msg_send_no_type_checking(env, (actor, selector));
            writeln!(
                writer,
                "    getter hungerLevel={value} (0x{value:x}, f32={:.3})",
                f32::from_bits(value)
            )?;
        }
    }

    let eat_date = read_object_ivar(env, actor, "eatDate").or_else(|| {
        let selector = env.objc.lookup_selector("eatDate")?;
        if env.objc.object_has_method(&env.mem, actor, selector) {
            Some(msg_send_no_type_checking(env, (actor, selector)))
        } else {
            None
        }
    });
    if let Some(eat_date) = eat_date {
        writeln!(writer, "    eatDate={}", date_to_debug(env, eat_date))?;

        if let Some(selector) = env.objc.lookup_selector("timeIntervalSinceDate:") {
            let now_class = env.objc.get_known_class("NSDate", &mut env.mem);
            if let Some(date_selector) = env.objc.lookup_selector("date") {
                if env
                    .objc
                    .object_has_method(&env.mem, now_class, date_selector)
                {
                    let now: id = msg_send_no_type_checking(env, (now_class, date_selector));
                    if now != nil
                        && eat_date != nil
                        && env.objc.object_has_method(&env.mem, now, selector)
                    {
                        let elapsed: f64 =
                            msg_send_no_type_checking(env, (now, selector, eat_date));
                        writeln!(writer, "    elapsedSinceEatDate={elapsed:.0}s")?;
                    }
                }
            }
        }
    } else {
        writeln!(writer, "    eatDate=n/a")?;
    }

    Ok(())
}

fn get_zombie_menu(env: &mut Environment) -> Option<id> {
    if env.bundle.bundle_identifier() == "com.playforge.ZombieFarm2" {
        return None;
    }

    let zombie_menu_class = env.objc.get_known_class("ZFZombieMenu", &mut env.mem);
    let zombie_menu_selector = env.objc.lookup_selector("zombieMenu")?;
    if !env
        .objc
        .object_has_method(&env.mem, zombie_menu_class, zombie_menu_selector)
    {
        return None;
    }
    let zombie_menu: id = msg_send_no_type_checking(env, (zombie_menu_class, zombie_menu_selector));
    (zombie_menu != nil).then_some(zombie_menu)
}

fn dump_zombie_menu(env: &mut Environment, writer: &mut dyn Write) -> IoResult<()> {
    writeln!(writer, "ZFZombieMenu:")?;
    let Some(menu) = get_zombie_menu(env) else {
        writeln!(writer, "  (not available)")?;
        return Ok(());
    };
    writeln!(writer, "  menu={}", object_to_debug(env, menu))?;

    let current_zombie_getter = env
        .objc
        .lookup_selector("currentZombie")
        .and_then(|selector| {
            env.objc
                .object_has_method(&env.mem, menu, selector)
                .then(|| msg_send_no_type_checking(env, (menu, selector)))
        });
    let current_zombie_ivar = read_object_ivar(env, menu, "currentZombie");
    writeln!(
        writer,
        "  currentZombie(getter)={}",
        current_zombie_getter
            .map(|object| object_to_debug(env, object))
            .unwrap_or_else(|| "n/a".to_string())
    )?;
    writeln!(
        writer,
        "  currentZombie(ivar)={}",
        current_zombie_ivar
            .map(|object| object_to_debug(env, object))
            .unwrap_or_else(|| "n/a".to_string())
    )?;
    if let Some(current_zombie) = current_zombie_getter.or(current_zombie_ivar) {
        dump_actor_summary(env, writer, current_zombie)?;
    }

    for ivar_name in [
        "nameLabel",
        "statusValLabel",
        "hungerLabel",
        "hungerValLabel",
        "typeValLabel",
        "invasionsValLabel",
    ] {
        let Some(label) = read_object_ivar(env, menu, ivar_name) else {
            writeln!(writer, "  {ivar_name}=n/a")?;
            continue;
        };
        writeln!(
            writer,
            "  {ivar_name}={} {}",
            object_to_debug(env, label),
            get_label_string(env, label)
        )?;
    }

    Ok(())
}

pub fn write_actor_snapshot(env: &mut Environment, mut writer: impl Write) -> IoResult<()> {
    writeln!(writer, "== Zombie Farm Actor Inspector ==")?;
    if !env
        .bundle
        .bundle_identifier()
        .starts_with("com.playforge.Z")
    {
        writeln!(writer, "(not a Zombie Farm bundle)")?;
        return Ok(());
    }

    let regs = *env.cpu.regs();
    let result = (|| -> IoResult<()> {
        dump_game_time_state(env, &mut writer)?;
        let game_state_actor_list = get_actor_list_from_game_state(env);
        let actor_manager_actor_list = get_actor_list_from_actor_manager(env);
        dump_actor_list(
            env,
            &mut writer,
            "GameState.zfGameData.actorList:",
            game_state_actor_list,
        )?;
        dump_actor_list(
            env,
            &mut writer,
            "ZFActorManager.actorList:",
            actor_manager_actor_list,
        )?;
        dump_zombie_menu(env, &mut writer)?;
        dump_cocos_scene(env, &mut writer)?;
        Ok(())
    })();
    env.cpu.regs_mut().copy_from_slice(&regs);

    result
}
