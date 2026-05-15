/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! `UITableView` and `UITableViewCell`.

use crate::frameworks::core_graphics::{CGFloat, CGRect};
use crate::frameworks::foundation::ns_array;
use crate::frameworks::foundation::NSInteger;
use crate::objc::{
    id, impl_HostObject_with_superclass, msg, msg_class, msg_super, nil, objc_classes, release,
    retain, ClassExports, NSZonePtr,
};

type UITableViewStyle = NSInteger;

struct UITableViewHostObject {
    superclass: super::UIScrollViewHostObject,
    data_source: id,
    table_header_view: id,
    table_footer_view: id,
    background_view: id,
    row_height: CGFloat,
    style: UITableViewStyle,
    editing: bool,
    allows_selection: bool,
}
impl_HostObject_with_superclass!(UITableViewHostObject);
impl Default for UITableViewHostObject {
    fn default() -> Self {
        Self {
            superclass: Default::default(),
            data_source: nil,
            table_header_view: nil,
            table_footer_view: nil,
            background_view: nil,
            row_height: 44.0,
            style: 0,
            editing: false,
            allows_selection: true,
        }
    }
}

struct UITableViewCellHostObject {
    superclass: super::super::UIViewHostObject,
    reuse_identifier: id,
    content_view: id,
    text_label: id,
    detail_text_label: id,
    image_view: id,
}
impl_HostObject_with_superclass!(UITableViewCellHostObject);
impl Default for UITableViewCellHostObject {
    fn default() -> Self {
        Self {
            superclass: Default::default(),
            reuse_identifier: nil,
            content_view: nil,
            text_label: nil,
            detail_text_label: nil,
            image_view: nil,
        }
    }
}

fn ensure_default_subviews(env: &mut crate::Environment, this: id) {
    if env
        .objc
        .borrow::<UITableViewCellHostObject>(this)
        .content_view
        != nil
    {
        return;
    }

    let bounds: CGRect = msg![env; this bounds];

    let content_view: id = msg_class![env; UIView alloc];
    let content_view: id = msg![env; content_view initWithFrame:bounds];

    let image_view: id = msg_class![env; UIImageView alloc];
    let image_view: id = msg![env; image_view initWithFrame:bounds];

    let text_label: id = msg_class![env; UILabel alloc];
    let text_label: id = msg![env; text_label initWithFrame:bounds];
    () = msg![env; text_label setBackgroundColor:nil];

    let detail_text_label: id = msg_class![env; UILabel alloc];
    let detail_text_label: id = msg![env; detail_text_label initWithFrame:bounds];
    () = msg![env; detail_text_label setBackgroundColor:nil];

    () = msg![env; content_view addSubview:image_view];
    () = msg![env; content_view addSubview:text_label];
    () = msg![env; content_view addSubview:detail_text_label];
    () = msg![env; this addSubview:content_view];

    {
        let host = env.objc.borrow_mut::<UITableViewCellHostObject>(this);
        host.content_view = content_view;
        host.image_view = image_view;
        host.text_label = text_label;
        host.detail_text_label = detail_text_label;
    }
}

pub const CLASSES: ClassExports = objc_classes! {

(env, this, _cmd);

@implementation UITableView: UIScrollView

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::<UITableViewHostObject>::default();
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

- (id)initWithFrame:(CGRect)frame {
    let this: id = msg![env; this initWithFrame:frame style:0];
    this
}

- (id)initWithFrame:(CGRect)frame style:(UITableViewStyle)style {
    let this: id = msg_super![env; this initWithFrame:frame];
    let host = env.objc.borrow_mut::<UITableViewHostObject>(this);
    host.style = style;
    host.row_height = 44.0;
    this
}

- (id)initWithCoder:(id)coder {
    let this: id = msg_super![env; this initWithCoder:coder];
    env.objc.borrow_mut::<UITableViewHostObject>(this).row_height = 44.0;
    this
}

- (())dealloc {
    let UITableViewHostObject {
        superclass: _,
        data_source: _,
        table_header_view,
        table_footer_view,
        background_view,
        row_height: _,
        style: _,
        editing: _,
        allows_selection: _,
    } = std::mem::take(env.objc.borrow_mut(this));
    release(env, table_header_view);
    release(env, table_footer_view);
    release(env, background_view);
    msg_super![env; this dealloc]
}

- (id)dataSource {
    env.objc.borrow::<UITableViewHostObject>(this).data_source
}
- (())setDataSource:(id)data_source {
    env.objc.borrow_mut::<UITableViewHostObject>(this).data_source = data_source;
}

- (CGFloat)rowHeight {
    env.objc.borrow::<UITableViewHostObject>(this).row_height
}
- (())setRowHeight:(CGFloat)row_height {
    env.objc.borrow_mut::<UITableViewHostObject>(this).row_height = row_height;
}

- (UITableViewStyle)style {
    env.objc.borrow::<UITableViewHostObject>(this).style
}

- (bool)isEditing {
    env.objc.borrow::<UITableViewHostObject>(this).editing
}
- (())setEditing:(bool)editing {
    env.objc.borrow_mut::<UITableViewHostObject>(this).editing = editing;
}
- (())setEditing:(bool)editing animated:(bool)_animated {
    env.objc.borrow_mut::<UITableViewHostObject>(this).editing = editing;
}

- (bool)allowsSelection {
    env.objc.borrow::<UITableViewHostObject>(this).allows_selection
}
- (())setAllowsSelection:(bool)allows_selection {
    env.objc.borrow_mut::<UITableViewHostObject>(this).allows_selection = allows_selection;
}

- (id)tableHeaderView {
    env.objc.borrow::<UITableViewHostObject>(this).table_header_view
}
- (())setTableHeaderView:(id)view {
    let old = {
        let host = env.objc.borrow_mut::<UITableViewHostObject>(this);
        std::mem::replace(&mut host.table_header_view, view)
    };
    retain(env, view);
    release(env, old);
    if view != nil {
        () = msg![env; this addSubview:view];
    }
}

- (id)tableFooterView {
    env.objc.borrow::<UITableViewHostObject>(this).table_footer_view
}
- (())setTableFooterView:(id)view {
    let old = {
        let host = env.objc.borrow_mut::<UITableViewHostObject>(this);
        std::mem::replace(&mut host.table_footer_view, view)
    };
    retain(env, view);
    release(env, old);
    if view != nil {
        () = msg![env; this addSubview:view];
    }
}

- (id)backgroundView {
    env.objc.borrow::<UITableViewHostObject>(this).background_view
}
- (())setBackgroundView:(id)view {
    let old = {
        let host = env.objc.borrow_mut::<UITableViewHostObject>(this);
        std::mem::replace(&mut host.background_view, view)
    };
    retain(env, view);
    release(env, old);
    if view != nil {
        () = msg![env; this addSubview:view];
    }
}

- (id)visibleCells {
    ns_array::from_vec(env, Vec::new())
}

- (id)indexPathForSelectedRow {
    nil
}

- (id)dequeueReusableCellWithIdentifier:(id)_reuse_identifier {
    nil
}
- (id)dequeueReusableCellWithIdentifier:(id)_reuse_identifier forIndexPath:(id)_index_path {
    nil
}

- (())reloadData {
    () = msg![env; this setNeedsLayout];
}

- (())layoutSubviews {
    msg_super![env; this layoutSubviews]
}

- (())selectRowAtIndexPath:(id)_index_path animated:(bool)_animated scrollPosition:(NSInteger)_scroll_position {
}
- (())deselectRowAtIndexPath:(id)_index_path animated:(bool)_animated {
}
- (())scrollToRowAtIndexPath:(id)_index_path atScrollPosition:(NSInteger)_scroll_position animated:(bool)_animated {
}

- (())setSeparatorStyle:(NSInteger)_separator_style {
}
- (())setSectionHeaderHeight:(CGFloat)_height {
}
- (())setSectionFooterHeight:(CGFloat)_height {
}
- (())setAllowsSelectionDuringEditing:(bool)_allows_selection_during_editing {
}

@end

@implementation UITableViewCell: UIView

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::<UITableViewCellHostObject>::default();
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

- (id)initWithFrame:(CGRect)frame {
    let this: id = msg_super![env; this initWithFrame:frame];
    ensure_default_subviews(env, this);
    this
}

- (id)initWithStyle:(NSInteger)_style reuseIdentifier:(id)reuse_identifier {
    let frame = CGRect::default();
    let this: id = msg_super![env; this initWithFrame:frame];
    ensure_default_subviews(env, this);
    retain(env, reuse_identifier);
    let old = {
        let host = env.objc.borrow_mut::<UITableViewCellHostObject>(this);
        std::mem::replace(&mut host.reuse_identifier, reuse_identifier)
    };
    release(env, old);
    this
}

- (())dealloc {
    let UITableViewCellHostObject {
        superclass: _,
        reuse_identifier,
        content_view,
        text_label,
        detail_text_label,
        image_view,
    } = std::mem::take(env.objc.borrow_mut(this));
    release(env, reuse_identifier);
    release(env, content_view);
    release(env, text_label);
    release(env, detail_text_label);
    release(env, image_view);
    msg_super![env; this dealloc]
}

- (id)reuseIdentifier {
    env.objc.borrow::<UITableViewCellHostObject>(this).reuse_identifier
}

- (id)contentView {
    ensure_default_subviews(env, this);
    env.objc.borrow::<UITableViewCellHostObject>(this).content_view
}

- (id)textLabel {
    ensure_default_subviews(env, this);
    env.objc.borrow::<UITableViewCellHostObject>(this).text_label
}

- (id)detailTextLabel {
    ensure_default_subviews(env, this);
    env.objc.borrow::<UITableViewCellHostObject>(this).detail_text_label
}

- (id)imageView {
    ensure_default_subviews(env, this);
    env.objc.borrow::<UITableViewCellHostObject>(this).image_view
}

- (())prepareForReuse {
}
- (())setSelected:(bool)_selected {
}
- (())setSelected:(bool)_selected animated:(bool)_animated {
}
- (())setHighlighted:(bool)_highlighted {
}
- (())setHighlighted:(bool)_highlighted animated:(bool)_animated {
}

@end

};
