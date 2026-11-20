//! Event-driven keyboard overlays. Closed overlays allocate and draw nothing.
use crate::{
    render::GavnoRenderElement,
    topbar::Canvas,
    ui::text::{TextStyleSpec, Weight},
    Gavno,
};
use smithay::{
    backend::renderer::gles::GlesRenderer,
    desktop::{Space, Window},
    output::Output,
    utils::{IsAlive, Logical, Rectangle},
};

pub const LETTERS: &str = "asdfghjklqwertyuiopzxcvbnm";

pub struct WindowHints {
    pub output: Output,
    pub windows: Vec<(String, Window)>,
    prefix: String,
}

#[derive(Default)]
pub struct KeyboardOverlay {
    pub windows: Option<WindowHints>,
    sheet: Option<(Output, Vec<(String, String)>)>,
    canvas: Canvas,
}

/// Fixed-length labels keep even more than 26 windows reachable without ambiguity.
pub fn labels(count: usize) -> Vec<String> {
    let letters: Vec<char> = LETTERS.chars().collect();
    let mut digits = 1;
    let mut capacity = letters.len();
    while capacity < count {
        digits += 1;
        capacity *= letters.len();
    }
    (0..count)
        .map(|mut i| {
            let mut label = vec![letters[0]; digits];
            for c in label.iter_mut().rev() {
                *c = letters[i % letters.len()];
                i /= letters.len();
            }
            label.into_iter().collect()
        })
        .collect()
}

impl KeyboardOverlay {
    pub fn open(&self) -> bool {
        self.windows.is_some() || self.sheet.is_some()
    }

    pub fn output(&self) -> Option<&Output> {
        self.windows.as_ref().map(|hints| &hints.output)
            .or_else(|| self.sheet.as_ref().map(|(output, _)| output))
    }

    pub fn elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        geo: Rectangle<i32, Logical>,
        space: &Space<Window>,
        scale: f64,
        accent: [f32; 4],
    ) -> Vec<GavnoRenderElement> {
        if !self.open() {
            return Vec::new();
        }
        self.canvas.begin(renderer, scale);
        if let Some(hints) = self.windows.as_ref().filter(|h| &h.output == output) {
            for (letter, window) in &hints.windows {
                if !window.alive() {
                    continue;
                }
                let Some(loc) = space.element_location(window) else {
                    continue;
                };
                let size = window.geometry().size;
                let width = (letter.len() as i32 * 27 + 24).max(64);
                let x = loc.x - geo.loc.x + (size.w - width) / 2;
                let y = loc.y - geo.loc.y + (size.h - 64) / 2;
                let badge = Rectangle::new((x, y).into(), (width, 64).into());
                self.canvas.shadow(badge, 14.0);
                self.canvas
                    .rect(badge, 14.0, accent, 1.0, crate::topbar::LINE);
                self.canvas.styled_label(
                    renderer,
                    &letter.to_uppercase(),
                    x + 18,
                    y + 10,
                    width - 24,
                    TextStyleSpec {
                        size: 34.0,
                        weight: Weight::SemiBold,
                        color: [0.06, 0.07, 0.10, 1.0],
                        tracking: 0.0,
                    },
                );
            }
        }
        if let Some((_, rows)) = self.sheet.as_ref().filter(|(o, _)| o == output) {
            let width = (geo.size.w - 48).clamp(1, 1120);
            let available = (geo.size.h - 120).max(80);
            let row_height = 23;
            let per_column = (available / row_height).max(1) as usize;
            let columns = rows.len().div_ceil(per_column).max(1);
            let per_column = rows.len().div_ceil(columns).max(1);
            let height = per_column as i32 * row_height + 76;
            let x = (geo.size.w - width) / 2;
            let y = (geo.size.h - height) / 2;
            let panel = Rectangle::new((x, y).into(), (width, height).into());
            self.canvas.shadow(panel, 18.0);
            self.canvas
                .glass(panel, 18.0, crate::ui::glass::tone::THICK, crate::topbar::GLASS, 1.0, crate::topbar::LINE);
            self.canvas.label(
                renderer,
                "Keyboard shortcuts · any key closes",
                x + 24,
                y + 16,
                width - 48,
                18.0,
            );
            let col_width = (width - 32) / columns as i32;
            for (i, (keys, label)) in rows.iter().enumerate() {
                let cx = x + 18 + (i / per_column) as i32 * col_width;
                let cy = y + 54 + (i % per_column) as i32 * row_height;
                self.canvas.styled_label(
                    renderer,
                    &format!("{keys}  {label}"),
                    cx,
                    cy,
                    col_width - 12,
                    TextStyleSpec {
                        size: 11.5,
                        weight: if keys.is_empty() {
                            Weight::SemiBold
                        } else {
                            Weight::Regular
                        },
                        color: if keys.is_empty() {
                            accent
                        } else {
                            [0.96, 0.95, 0.98, 0.92]
                        },
                        tracking: 0.0,
                    },
                );
            }
        }
        self.canvas.finish()
    }
}

impl Gavno {
    pub fn toggle_window_hints(&mut self) {
        if self.overlay.keyboard.windows.is_some() {
            self.close_keyboard_overlay();
            return;
        }
        let Some(output) = self.active_output() else {
            return;
        };
        let Some(owner) = self.workspaces.get(&output) else {
            return;
        };
        let windows: Vec<_> = owner.list.items[owner.list.active]
            .windows
            .iter()
            .filter(|v| {
                v.window.alive()
                    && !crate::window_mode::mode_state(&v.window).minimized
                    && self.space.element_location(&v.window).is_some()
            })
            .map(|v| v.window.clone())
            .collect();
        self.close_overview();
        self.exit_resize_mode();
        self.overlay.keyboard.sheet = None;
        self.overlay.keyboard.windows = Some(WindowHints {
            output,
            windows: labels(windows.len()).into_iter().zip(windows).collect(),
            prefix: String::new(),
        });
        self.sync_bar_hints();
        self.cancel_shell_tap();
        self.queue_redraw_all();
    }

    pub fn toggle_cheat_sheet(&mut self) {
        if self.overlay.keyboard.sheet.is_some() {
            self.close_keyboard_overlay();
            return;
        }
        let Some(output) = self.active_output() else {
            return;
        };
        let rows = crate::hints::cheat_rows(self);
        self.overlay.keyboard.windows = None;
        self.overlay.keyboard.sheet = Some((output, rows));
        self.sync_bar_hints();
        self.cancel_shell_tap();
        self.queue_redraw_all();
    }

    pub fn close_keyboard_overlay(&mut self) {
        if !self.overlay.keyboard.open() {
            return;
        }
        self.overlay.keyboard.windows = None;
        self.overlay.keyboard.sheet = None;
        self.sync_bar_hints();
        self.queue_redraw_all();
    }

    pub fn keyboard_overlay_key(&mut self, sym: u32) {
        let mut target = None;
        if let Some(hints) = &mut self.overlay.keyboard.windows {
            if let Some(c) = char::from_u32(sym)
                .map(|c| c.to_ascii_lowercase())
                .filter(|c| LETTERS.contains(*c))
            {
                hints.prefix.push(c);
                target = hints
                    .windows
                    .iter()
                    .find(|(s, _)| *s == hints.prefix)
                    .map(|(_, w)| w.clone());
                if target.is_none()
                    && hints
                        .windows
                        .iter()
                        .any(|(s, _)| s.starts_with(&hints.prefix))
                {
                    return;
                }
            }
        }
        self.close_keyboard_overlay();
        if let Some(window) =
            target.filter(|w| w.alive() && self.space.element_location(w).is_some())
        {
            self.focus_window(Some(&window));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn labels_start_at_home_row_and_remain_unique_beyond_one_alphabet() {
        assert_eq!(labels(26).join(""), LETTERS);
        let labels = labels(50);
        assert!(labels.iter().all(|s| s.len() == 2));
        let mut unique = labels.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), labels.len());
    }
}
