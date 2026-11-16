use crate::{model, thumbs::Thumbnails};
use gtk::prelude::*;
use std::rc::Rc;
pub type Rename = Rc<dyn Fn(gio::FileInfo, String)>;
pub type Report = Rc<dyn Fn(String)>;

pub struct Views {
    pub stack: gtk::Stack,
    pub selection: gtk::MultiSelection,
    pub sorted: gtk::SortListModel,
    pub grid: gtk::GridView,
    pub list: gtk::ColumnView,
}
impl Views {
    pub fn new(
        model: &impl IsA<gio::ListModel>,
        thumbs: Thumbnails,
        rename: Rename,
        report: Report,
    ) -> Self {
        let sorted = gtk::SortListModel::new(Some(model.clone()), None::<gtk::Sorter>);
        sorted.set_incremental(true);
        let selection = gtk::MultiSelection::new(Some(sorted.clone()));
        let factory = name_factory(true, thumbs.clone(), rename.clone(), report.clone());
        let grid = gtk::GridView::new(Some(selection.clone()), Some(factory));
        grid.set_min_columns(1);
        grid.set_max_columns(20);
        grid.set_single_click_activate(false);
        grid.add_css_class("files-grid");
        let list = gtk::ColumnView::new(Some(selection.clone()));
        list.set_single_click_activate(false);
        list.set_show_column_separators(false);
        list.set_show_row_separators(false);
        list.add_css_class("files-list");
        let name = gtk::ColumnViewColumn::new(
            Some("Name"),
            Some(name_factory(false, thumbs, rename, report)),
        );
        name.set_expand(true);
        name.set_resizable(true);
        name.set_sorter(Some(&sorter("name")));
        list.append_column(&name);
        for (title, field, width) in [
            ("Size", "size", 100),
            ("Modified", "modified", 160),
            ("Type", "type", 150),
        ] {
            let factory = gtk::SignalListItemFactory::new();
            factory.connect_setup(|_, object| {
                if let Some(item) = object.downcast_ref::<gtk::ListItem>() {
                    let label = gtk::Label::new(None);
                    label.set_xalign(0.);
                    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
                    label.add_css_class("dim-label");
                    item.set_child(Some(&label));
                }
            });
            factory.connect_bind(move |_, object| {
                let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
                    return;
                };
                let Some(info) = item.item().and_downcast::<gio::FileInfo>() else {
                    return;
                };
                let Some(label) = item.child().and_downcast::<gtk::Label>() else {
                    return;
                };
                label.set_text(&match field {
                    "size" => {
                        if model::is_dir(&info) {
                            "—".into()
                        } else {
                            model::format_size(info.size().max(0) as u64)
                        }
                    }
                    "modified" => model::modified(&info),
                    _ => model::kind(&info),
                });
            });
            let col = gtk::ColumnViewColumn::new(Some(title), Some(factory));
            col.set_fixed_width(width);
            col.set_resizable(true);
            col.set_sorter(Some(&sorter(field)));
            list.append_column(&col);
        }
        list.sort_by_column(Some(&name), gtk::SortType::Ascending);
        let sort = gtk::MultiSorter::new();
        sort.append(gtk::CustomSorter::new(|a, b| {
            let (Some(a), Some(b)) = (
                a.downcast_ref::<gio::FileInfo>(),
                b.downcast_ref::<gio::FileInfo>(),
            ) else {
                return gtk::Ordering::Equal;
            };
            model::is_dir(b).cmp(&model::is_dir(a)).into()
        }));
        if let Some(columns) = list.sorter() {
            sort.append(columns);
        }
        sort.append(sorter("name"));
        sorted.set_sorter(Some(&sort));
        let stack = gtk::Stack::new();
        stack.set_vexpand(true);
        stack.set_hexpand(true);
        for (name, widget) in [
            ("grid", grid.clone().upcast::<gtk::Widget>()),
            ("list", list.clone().upcast()),
        ] {
            let scroll = gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .child(&widget)
                .build();
            stack.add_named(&scroll, Some(name));
        }
        Self {
            stack,
            selection,
            sorted,
            grid,
            list,
        }
    }
    pub fn selected(&self) -> Vec<gio::FileInfo> {
        let selected = self.selection.selection();
        let mut items = Vec::new();
        for i in 0..self.selection.n_items() {
            if selected.contains(i) {
                if let Some(info) = self.selection.item(i).and_downcast::<gio::FileInfo>() {
                    items.push(info);
                }
            }
        }
        items
    }
    pub fn rename_selected(&self) {
        if let Some(info) = self.selected().first() {
            let uri = model::file(info).uri();
            start_rename(self.stack.upcast_ref(), &uri);
        }
    }
}
fn start_rename(widget: &gtk::Widget, uri: &str) -> bool {
    if let Some(label) = widget.downcast_ref::<gtk::EditableLabel>() {
        if label.widget_name() == uri {
            label.start_editing();
            label.select_region(0, -1);
            return true;
        }
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        if start_rename(&current, uri) {
            return true;
        }
        child = current.next_sibling();
    }
    false
}
fn sorter(field: &'static str) -> gtk::CustomSorter {
    gtk::CustomSorter::new(move |a, b| {
        let (Some(a), Some(b)) = (
            a.downcast_ref::<gio::FileInfo>(),
            b.downcast_ref::<gio::FileInfo>(),
        ) else {
            return gtk::Ordering::Equal;
        };
        match field {
            "size" => a.size().cmp(&b.size()),
            "modified" => a
                .attribute_uint64("time::modified")
                .cmp(&b.attribute_uint64("time::modified")),
            "type" => model::natural_cmp(&model::kind(a), &model::kind(b)),
            _ => model::natural_cmp(a.display_name().as_str(), b.display_name().as_str()),
        }
        .into()
    })
}
fn name_factory(
    grid: bool,
    thumbs: Thumbnails,
    rename: Rename,
    report: Report,
) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, object| {
        let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let row = gtk::Box::new(
            if grid {
                gtk::Orientation::Vertical
            } else {
                gtk::Orientation::Horizontal
            },
            if grid { 8 } else { 10 },
        );
        row.add_css_class("file-tile");
        if grid {
            row.set_width_request(112);
            row.set_height_request(108);
        }
        let image = gtk::Image::new();
        image.set_pixel_size(if grid { 56 } else { 24 });
        if grid {
            image.set_halign(gtk::Align::Center);
        }
        let label = gtk::EditableLabel::new("");
        label.set_hexpand(true);
        label.set_halign(if grid {
            gtk::Align::Center
        } else {
            gtk::Align::Fill
        });
        label.set_width_chars(1);
        label.set_max_width_chars(if grid { 16 } else { 120 });
        label.connect_editing_notify(glib::clone!(
            #[weak]
            item,
            #[strong]
            rename,
            move |label| {
                if label.is_editing() {
                    return;
                }
                if let Some(info) = item.item().and_downcast::<gio::FileInfo>() {
                    let text = label.text().to_string();
                    if text != info.display_name().as_str() {
                        rename(info, text);
                    }
                }
            }
        ));
        row.append(&image);
        row.append(&label);
        item.set_child(Some(&row));
    });
    factory.connect_bind(move |_, object| {
        let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(info) = item.item().and_downcast::<gio::FileInfo>() else {
            return;
        };
        let Some(row) = item.child() else { return };
        let Some(image) = row.first_child().and_downcast::<gtk::Image>() else {
            return;
        };
        let Some(label) = row.last_child().and_downcast::<gtk::EditableLabel>() else {
            return;
        };
        label.set_text(info.display_name().as_str());
        label.set_widget_name(model::file(&info).uri().as_str());
        row.set_tooltip_text(Some(&format!(
            "{}\n{}",
            info.display_name(),
            model::file(&info).parse_name()
        )));
        if model::is_dir(&info) {
            image.set_icon_name(Some("folder-symbolic"));
            image.add_css_class("folder-icon");
        } else {
            image.remove_css_class("folder-icon");
            if let Some(icon) = info.icon() {
                image.set_from_gicon(&icon);
            } else {
                image.set_icon_name(Some("text-x-generic-symbolic"));
            }
            if grid && crate::thumbs::supported(&info) {
                let uri = model::file(&info).uri();
                glib::spawn_future_local(glib::clone!(
                    #[weak]
                    image,
                    #[weak]
                    item,
                    #[strong]
                    thumbs,
                    #[strong]
                    report,
                    async move {
                        match thumbs.get(&info).await {
                            Ok(Some(texture)) => {
                                if item
                                    .item()
                                    .and_downcast::<gio::FileInfo>()
                                    .is_some_and(|i| model::file(&i).uri() == uri)
                                {
                                    image.set_paintable(Some(&texture));
                                }
                            }
                            Ok(None) => {}
                            Err(e) => report(format!("Thumbnail: {e}")),
                        }
                    }
                ));
            }
        }
    });
    factory
}
