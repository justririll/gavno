//! Media painters use the same logical rectangles as their pointer targets.
use super::{Command, Playback, Player, Snapshot};
use crate::{
    render::GavnoRenderElement,
    topbar::{
        geometry::{rect, Item, Rect},
        Canvas, LINE,
    },
};
use smithay::{
    backend::renderer::{
        element::{memory::MemoryRenderBufferRenderElement, Kind},
        gles::GlesRenderer,
    },
    utils::{Logical, Rectangle, Size},
};

pub fn item_command(item: Item) -> Option<Command> {
    match item {
        Item::MediaPrevious => Some(Command::Previous),
        Item::MediaPlayPause => Some(Command::PlayPause),
        Item::MediaNext => Some(Command::Next),
        _ => None,
    }
}
/// Themed symbolic icon for a media command. The UI fonts have no media
/// glyphs (U+23EE..), so text labels would render as missing-glyph boxes.
pub fn icon(player: &Player, command: Command) -> &'static str {
    match command {
        Command::Previous => "media-skip-backward-symbolic",
        Command::Next => "media-skip-forward-symbolic",
        Command::PlayPause => {
            if player.playback == Playback::Playing {
                "media-playback-pause-symbolic"
            } else {
                "media-playback-start-symbolic"
            }
        }
    }
}
/// Light tint for media icons on the dark glass panels.
pub const ICON_COLOR: [u8; 3] = [236, 238, 244];
pub fn info(c: &mut Canvas, renderer: &mut GlesRenderer, snapshot: &Snapshot, area: Rect) {
    let Some(player) = &snapshot.player else {
        return;
    };
    let image = rect(area.loc.x, area.loc.y + 2, 48, 48);
    if let Some(buffer) = &snapshot.image {
        let dest = c.mapped(image);
        if let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            (
                (dest.loc.x as f64 * c.scale).round(),
                (dest.loc.y as f64 * c.scale).round(),
            ),
            buffer,
            Some(c.alpha),
            Some(Rectangle::from_size((96.0, 96.0).into())),
            Some(dest.size),
            Kind::Unspecified,
        ) {
            c.elements.push(GavnoRenderElement::Memory(element));
        }
    } else {
        c.rect(image, 10.0, [1.0, 1.0, 1.0, 0.08], 0.0, LINE);
        c.label(renderer, "♫", image.loc.x + 14, image.loc.y + 9, 28, 22.0);
    }
    c.label(
        renderer,
        player.title(),
        area.loc.x + 60,
        area.loc.y + 4,
        area.size.w - 60,
        13.0,
    );
    c.label(
        renderer,
        &player.artist(),
        area.loc.x + 60,
        area.loc.y + 28,
        area.size.w - 60,
        11.5,
    );
}
pub fn button(
    c: &mut Canvas,
    renderer: &mut GlesRenderer,
    icons: &mut crate::icons::Icons,
    theme: &str,
    player: &Player,
    command: Command,
    area: Rect,
    hover: bool,
) {
    let alpha = c.alpha;
    if !player.allows(command) {
        c.alpha *= 0.35;
    }
    c.rect(
        area,
        10.0,
        [1.0, 1.0, 1.0, if hover { 0.16 } else { 0.065 }],
        0.0,
        LINE,
    );
    let side = 16.min(area.size.w).min(area.size.h);
    c.colored_icon(
        renderer,
        icons,
        &[icon(player, command)],
        theme,
        rect(area.loc.x + (area.size.w - side) / 2, area.loc.y + (area.size.h - side) / 2, side, side),
        Some(ICON_COLOR),
    );
    c.alpha = alpha;
}
/// Beneath the clock/date, above the password card. No authentication state lives here.
pub fn lock_geometry(size: Size<i32, Logical>) -> Option<(Rect, [(Command, Rect); 3])> {
    let width = 360.min(size.w - 32).max(0);
    let x = (size.w - width) / 2;
    let y = (size.h as f64 * 0.12).round() as i32 + 164;
    // On very short outputs, keep the password card's space available.
    if width < 200 || y + 56 >= (size.h as f64 * 0.43) as i32 {
        return None;
    }
    Some((
        rect(x, y, width, 22),
        std::array::from_fn(|i| {
            (
                [Command::Previous, Command::PlayPause, Command::Next][i],
                rect(size.w / 2 - 68 + i as i32 * 48, y + 26, 40, 30),
            )
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lock_controls_fit_between_clock_and_password_card() {
        for (width, height) in [(1280, 720), (1646, 926)] {
            let size = (width, height).into();
            let (line, buttons) = lock_geometry(size).unwrap();
            let panel = Rectangle::from_size(size);
            assert!(panel.contains_rect(line));
            for (i, (_, area)) in buttons.iter().enumerate() {
                assert!(panel.contains_rect(*area));
                assert!(area.loc.y >= line.loc.y + line.size.h);
                assert!(area.loc.y + area.size.h < (size.h as f64 * 0.43) as i32);
                for (_, other) in &buttons[i + 1..] {
                    assert!(!area.overlaps(*other));
                }
            }
        }
    }
}
