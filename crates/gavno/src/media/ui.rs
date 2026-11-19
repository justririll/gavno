//! Media painters use the same logical rectangles as their pointer targets.
use super::{Command, Playback, Player, Snapshot};
use crate::{
    render::GavnoRenderElement,
    topbar::{
        geometry::{rect, Rect},
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

/// Themed symbolic icon for a media command. The UI fonts have no media
/// glyphs (U+23EE..), so text labels would render as missing-glyph boxes.
pub fn icon(player: &Player, command: Command) -> &'static str {
    transport_icon(player.playback == Playback::Playing, command)
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
        rect(
            area.loc.x + (area.size.w - side) / 2,
            area.loc.y + (area.size.h - side) / 2,
            side,
            side,
        ),
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

#[cfg(test)]
pub(crate) mod artifacts {
    use crate::render::GavnoRenderElement;
    use smithay::backend::{
        egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay},
        renderer::gles::GlesRenderer,
    };
    pub fn renderer() -> GlesRenderer {
        let display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.unwrap();
        unsafe { GlesRenderer::new(EGLContext::new(&display).unwrap()) }.unwrap()
    }
    pub fn decoded_cover(path: &std::path::Path) -> std::sync::Arc<super::Cover> {
        std::sync::Arc::new(
            crate::media::music::worker::decode_cover(
                path.to_str().unwrap(),
                crate::media::music::worker::CoverKind::Hero,
            )
            .unwrap(),
        )
    }
    pub fn player() -> crate::media::Player {
        crate::media::Player {
            name: crate::media::MUSIC_PLAYER.into(),
            identity: "Spotify".into(),
            metadata: crate::media::Metadata {
                title: "A walk after dark".into(),
                artists: vec!["Nils Frahm".into()],
                ..Default::default()
            },
            playback: crate::media::Playback::Playing,
            can_play: true,
            can_pause: true,
            can_next: true,
            can_previous: true,
            ..Default::default()
        }
    }
    pub fn cover_path(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("gavno-{label}-{}.png", std::process::id()));
        let mut pixels = Vec::new();
        for y in 0..160 {
            for x in 0..160 {
                let t = (x + y) as f32 / 318.0;
                pixels.extend([
                    (210.0 * (1.0 - t) + 40.0 * t) as u8,
                    (50.0 * (1.0 - t) + 70.0 * t) as u8,
                    (80.0 * (1.0 - t) + 160.0 * t) as u8,
                    255,
                ]);
            }
        }
        let mut encoder = png::Encoder::new(std::fs::File::create(&path).unwrap(), 160, 160);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&pixels)
            .unwrap();
        path
    }
    pub fn capture(
        renderer: &mut GlesRenderer,
        elements: &[GavnoRenderElement],
        size: (i32, i32),
        scale: f64,
        name: &str,
    ) {
        let Some(directory) = std::env::var_os("GAVNO_MUSIC_TEST_ARTIFACTS") else {
            return;
        };
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        let (w, h) = (
            (size.0 as f64 * scale).round() as i32,
            (size.1 as f64 * scale).round() as i32,
        );
        let bytes = crate::render::capture_elements(renderer, elements, (w, h), scale).unwrap();
        let suffix = if scale == 1.0 { "" } else { "-1.35" };
        let mut encoder = png::Encoder::new(
            std::fs::File::create(directory.join(format!("{name}{suffix}.png"))).unwrap(),
            w as u32,
            h as u32,
        );
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&bytes)
            .unwrap();
    }
}

/// Tint of a grey cover (OKLab L 0.40, no chroma), sRGB.
pub const NEUTRAL_TINT: [f32; 3] = [0.28, 0.28, 0.28];
/// One rounded hero image and its worker-computed, straight sRGB tint.
#[derive(Clone, Debug, Default)]
pub struct Cover {
    pub buffer: smithay::backend::renderer::element::memory::MemoryRenderBuffer,
    pub tint: [f32; 3],
}
pub fn same_cover(a: &Option<std::sync::Arc<Cover>>, b: &Option<std::sync::Arc<Cover>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => std::sync::Arc::ptr_eq(a, b),
        _ => false,
    }
}
pub fn cover(c: &mut Canvas, renderer: &mut GlesRenderer, image: &Cover, area: Rect) {
    let hero = super::music::worker::CoverKind::Hero.size();
    let area = c.mapped(area);
    if let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        (
            (area.loc.x as f64 * c.scale).round(),
            (area.loc.y as f64 * c.scale).round(),
        ),
        &image.buffer,
        Some(c.alpha),
        Some(Rectangle::from_size((hero as f64, hero as f64).into())),
        Some(area.size),
        Kind::Unspecified,
    ) {
        c.elements.push(GavnoRenderElement::Memory(element));
    }
}
fn transport_icon(playing: bool, command: Command) -> &'static str {
    match command {
        Command::Previous => "media-skip-backward-symbolic",
        Command::Next => "media-skip-forward-symbolic",
        Command::PlayPause if playing => "media-playback-pause-symbolic",
        _ => "media-playback-start-symbolic",
    }
}
fn transport_icon_size(command: Command, diameter: i32) -> i32 {
    if command == Command::PlayPause {
        if diameter >= 44 {
            20
        } else {
            18
        }
    } else if diameter >= 36 {
        18
    } else {
        16
    }
}
/// Canvas transport shared with the music popover; hit rectangles stay with callers.
pub fn transport(
    c: &mut Canvas,
    renderer: &mut GlesRenderer,
    icons: &mut crate::icons::Icons,
    theme: &str,
    command: Command,
    playing: bool,
    enabled: bool,
    area: Rect,
    hover: bool,
) {
    let alpha = c.alpha;
    if !enabled {
        c.alpha *= 0.35;
    }
    let toggle = command == Command::PlayPause;
    let diameter = area.size.w.min(area.size.h);
    if toggle || hover {
        c.rect(
            area,
            diameter as f32 / 2.0,
            [1.0, 1.0, 1.0, if toggle { 0.96 } else { 0.10 }],
            0.0,
            LINE,
        );
    }
    let side = transport_icon_size(command, diameter);
    c.colored_icon(
        renderer,
        icons,
        &[transport_icon(playing, command)],
        theme,
        rect(
            area.loc.x + (area.size.w - side) / 2,
            area.loc.y + (area.size.h - side) / 2,
            side,
            side,
        ),
        Some(if toggle { [16, 18, 24] } else { [255; 3] }),
    );
    c.alpha = alpha;
}
/// Classic transport uses the exact rectangles returned by media_buttons().
pub fn layer_transport(
    layer: &mut crate::classic::cache::Layer,
    theme: &str,
    player: Option<&Player>,
    command: Command,
    area: Rect,
) {
    let alpha = if player.is_some_and(|p| p.allows(command)) {
        1.0
    } else {
        0.35
    };
    let toggle = command == Command::PlayPause;
    let diameter = area.size.w.min(area.size.h);
    if toggle {
        layer.panel(
            area,
            diameter as f32 / 2.0,
            [1.0, 1.0, 1.0, 0.96 * alpha],
            0.0,
        );
    }
    let side = transport_icon_size(command, diameter);
    layer.tinted_icon_alpha(
        transport_icon(
            player.is_some_and(|p| p.playback == Playback::Playing),
            command,
        ),
        theme,
        rect(
            area.loc.x + (area.size.w - side) / 2,
            area.loc.y + (area.size.h - side) / 2,
            side,
            side,
        ),
        if toggle { [16, 18, 24] } else { [255; 3] },
        alpha,
    );
}
pub(crate) fn capsule_styles(
    playback: Playback,
) -> (
    crate::ui::text::TextStyleSpec,
    crate::ui::text::TextStyleSpec,
) {
    use crate::ui::text::{TextStyleSpec, Weight};
    let paused = playback != Playback::Playing;
    (
        TextStyleSpec {
            size: 12.0,
            weight: Weight::Medium,
            color: [1.0, 1.0, 1.0, if paused { 0.6 } else { 0.95 }],
            tracking: 0.0,
        },
        TextStyleSpec {
            size: 12.0,
            weight: Weight::Regular,
            color: [1.0, 1.0, 1.0, if paused { 0.4 } else { 0.55 }],
            tracking: 0.0,
        },
    )
}
/// The title keeps priority; when both parts are long it receives 65%.
fn capsule_labels(c: &mut Canvas, renderer: &mut GlesRenderer, player: &Player, area: Rect) {
    let (title_style, artist_style) = capsule_styles(player.playback);
    let artist = player.artist();
    let title_width = c.text.measure(player.title(), title_style).ceil() as i32;
    let artist_width = c.text.measure(&artist, artist_style).ceil() as i32;
    let separator_width = if artist.is_empty() {
        0
    } else {
        c.text.measure(" · ", artist_style).ceil() as i32
    };
    let width = area.size.w;
    let title_budget = if artist.is_empty() {
        width
    } else if title_width + artist_width + separator_width <= width {
        title_width
    } else {
        title_width.min((width * 65 / 100).max(width - artist_width - separator_width))
    };
    let title = crate::topbar::ellipsize(&mut c.text, player.title(), title_style, title_budget);
    let used = c.text.measure(&title, title_style).ceil() as i32;
    c.styled_label(
        renderer,
        &title,
        area.loc.x,
        area.loc.y,
        title_budget,
        title_style,
    );
    if !artist.is_empty() {
        c.styled_label(
            renderer,
            &format!(" · {artist}"),
            area.loc.x + used,
            area.loc.y,
            (width - used).max(0),
            artist_style,
        );
    }
}
pub fn capsule_content(
    c: &mut Canvas,
    renderer: &mut GlesRenderer,
    icons: &mut crate::icons::Icons,
    theme: &str,
    snapshot: &Snapshot,
    player: &Player,
    area: Rect,
    toggle_hover: bool,
) {
    let image = snapshot
        .music_image
        .as_ref()
        .filter(|_| player.name == super::MUSIC_PLAYER);
    if let Some(image) = image {
        let art = rect(area.loc.x + 5, area.loc.y + 3, 18, 18);
        cover(c, renderer, image, art);
        if toggle_hover {
            c.rect(art, 2.0, [0.0, 0.0, 0.0, 0.45], 0.0, LINE);
            c.colored_icon(
                renderer,
                icons,
                &[icon(player, Command::PlayPause)],
                theme,
                rect(art.loc.x + 3, art.loc.y + 3, 12, 12),
                Some([255; 3]),
            );
        }
    } else {
        c.colored_icon(
            renderer,
            icons,
            &[icon(player, Command::PlayPause)],
            theme,
            rect(area.loc.x + 9, area.loc.y + 4, 14, 14),
            Some(ICON_COLOR),
        );
    }
    capsule_labels(
        c,
        renderer,
        player,
        rect(area.loc.x + 30, area.loc.y + 3, area.size.w - 38, 20),
    );
}

#[cfg(test)]
mod capsule_style_tests {
    use super::*;
    #[test]
    fn paused_capsule_dims_both_parts_and_retains_their_weights() {
        use crate::ui::text::Weight;
        let (title, artist) = capsule_styles(Playback::Playing);
        assert_eq!(title.weight, Weight::Medium);
        assert_eq!(artist.weight, Weight::Regular);
        assert_eq!((title.color[3], artist.color[3]), (0.95, 0.55));
        let (title, artist) = capsule_styles(Playback::Paused);
        assert_eq!((title.color[3], artist.color[3]), (0.6, 0.4));
    }
}
