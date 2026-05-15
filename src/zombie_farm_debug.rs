use crate::frameworks::core_graphics::{CGPoint, CGSize};
use crate::objc::id;
use std::collections::{BTreeMap, VecDeque};
use std::io::{Result as IoResult, Write};
use std::sync::{Mutex, OnceLock};

const RECENT_LIMIT: usize = 160;

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
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

fn state() -> &'static Mutex<State> {
    STATE.get_or_init(|| Mutex::new(State::default()))
}

fn push_recent(recent: &mut VecDeque<String>, line: String) {
    if recent.len() >= RECENT_LIMIT {
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

    Ok(())
}
