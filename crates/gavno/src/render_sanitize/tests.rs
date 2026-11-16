use super::*;
use smithay::{
    backend::renderer::damage::OutputDamageTracker,
    utils::{Logical, Size},
};

const SCALES: [f64; 3] = [1.0, 1.5, 1.75];
const OUTPUT_SIZE: (i32, i32) = (800, 600);

#[derive(Clone)]
struct TestElement {
    id: Id,
    commit: CommitCounter,
    geometry: Rectangle<f64, Logical>,
    damage: Vec<Rectangle<f64, Logical>>,
    opaque: Vec<Rectangle<f64, Logical>>,
}

impl TestElement {
    fn new(geometry: Rectangle<f64, Logical>) -> Self {
        Self {
            id: Id::new(),
            commit: CommitCounter::default(),
            geometry,
            damage: vec![Rectangle::from_size(geometry.size)],
            opaque: Vec::new(),
        }
    }
}

impl Element for TestElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::new((2.0, 4.0).into(), (480.0, 400.0).into())
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geometry.to_physical(scale).to_i32_round()
    }

    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        // Deliberately distinct from geometry.loc to verify explicit delegation.
        self.geometry(scale).loc + Point::from((1, 2))
    }

    fn transform(&self) -> Transform {
        Transform::Flipped180
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        if commit == Some(self.commit) {
            return DamageSet::default();
        }
        self.damage
            .iter()
            .map(|r| r.to_physical(scale).to_i32_round())
            .collect()
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.opaque
            .iter()
            .map(|r| r.to_physical(scale).to_i32_round())
            .collect()
    }

    fn alpha(&self) -> f32 {
        0.75
    }

    fn kind(&self) -> Kind {
        Kind::Cursor
    }
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> Rectangle<f64, Logical> {
    Rectangle::new((x, y).into(), (w, h).into())
}

fn scene() -> Vec<TestElement> {
    let mut content = TestElement::new(rect(40.0, 40.0, 240.0, 200.0));
    content.damage = vec![
        rect(8.0, 8.0, 32.0, 0.0),
        rect(64.0, 48.0, 0.0, 24.0),
        // Positive logical sizes can disappear during physical rounding too.
        rect(112.0, 80.0, 16.0, 0.01),
        rect(144.0, 112.0, 0.01, 16.0),
        rect(176.0, 144.0, 24.0, 20.0),
    ];
    content.opaque = vec![rect(8.0, 8.0, 32.0, 0.0), rect(64.0, 48.0, 0.0, 24.0)];
    vec![
        content,
        TestElement::new(rect(360.0, 280.0, 80.0, 64.0)),
        TestElement::new(rect(80.0, 80.0, 0.0, 40.0)),
        TestElement::new(rect(160.0, 100.0, 40.0, 0.0)),
        TestElement::new(rect(200.0, 120.0, 0.01, 40.0)),
        TestElement::new(rect(240.0, 160.0, 40.0, 0.01)),
    ]
}

#[test]
fn filters_physical_rectangles_and_delegates_element_metadata() {
    for scale in SCALES.map(Scale::from) {
        let mut elements = scene();
        let element = &mut elements[0];
        element.commit.increment();
        element.opaque.push(rect(176.0, 144.0, 24.0, 20.0));
        let wrapped = SanitizedElement::new(&*element, scale).unwrap();
        assert!(std::ptr::eq(wrapped.id(), element.id()));
        assert_eq!(wrapped.current_commit(), element.current_commit());
        assert_eq!(wrapped.src(), element.src());
        assert_eq!(wrapped.geometry(scale), element.geometry(scale));
        assert_eq!(wrapped.location(scale), element.location(scale));
        assert_eq!(wrapped.transform(), element.transform());
        assert_eq!(wrapped.alpha(), element.alpha());
        assert_eq!(wrapped.kind(), element.kind());

        let expected_damage: Vec<_> = element
            .damage_since(scale, None)
            .into_iter()
            .filter(has_area)
            .collect();
        assert_eq!(expected_damage.len(), 1);
        assert_eq!(
            &*wrapped.damage_since(scale, None),
            expected_damage.as_slice()
        );
        assert!(wrapped.damage_since(scale, Some(element.commit)).is_empty());
        let expected_opaque: Vec<_> = element
            .opaque_regions(scale)
            .into_iter()
            .filter(has_area)
            .collect();
        assert_eq!(expected_opaque.len(), 1);
        assert_eq!(&*wrapped.opaque_regions(scale), expected_opaque.as_slice());

        for empty in &elements[2..] {
            assert!(SanitizedElement::new(empty, scale).is_none());
        }
    }
}

/// The rectangle conversion from Smithay 0.7's PlaneDamageClips::from_damage,
/// stopping before creating the DRM property blob (which requires a GPU).
fn plane_damage_clips(
    src: Rectangle<f64, Buffer>,
    dst: Rectangle<i32, Physical>,
    damage: &[Rectangle<i32, Physical>],
) -> Vec<[i32; 4]> {
    let scale = src.size
        / dst
            .size
            .to_logical(1)
            .to_buffer(1, Transform::Normal)
            .to_f64();
    damage
        .iter()
        .map(|rect| {
            let mut rect = rect
                .to_f64()
                .to_logical(1f64)
                .to_buffer(
                    1f64,
                    Transform::Normal,
                    &src.size.to_logical(1f64, Transform::Normal),
                )
                .upscale(scale);
            rect.loc += src.loc;
            let rect = rect.to_i32_up::<i32>();
            [
                rect.loc.x,
                rect.loc.y,
                rect.loc.x.saturating_add(rect.size.w),
                rect.loc.y.saturating_add(rect.size.h),
            ]
        })
        .collect()
}

fn assert_valid_damage(damage: &[Rectangle<i32, Physical>]) {
    let output = Rectangle::from_size(OUTPUT_SIZE.into());
    for rect in damage {
        assert!(has_area(rect), "empty output damage: {rect:?}");
        assert!(
            output.contains_rect(*rect),
            "damage outside output: {rect:?}"
        );
    }
    // Primary swapchain damage uses a full-size source buffer. Also exercise
    // scaling and source offsets, as used by other plane assignments.
    for (src, buffer_size) in [
        (
            Rectangle::<f64, Buffer>::from_size(Size::from((800.0, 600.0))),
            (800, 600),
        ),
        (
            Rectangle::new((8.0, 12.0).into(), (1600.0, 1200.0).into()),
            (1616, 1224),
        ),
    ] {
        for [x1, y1, x2, y2] in plane_damage_clips(src, output, damage) {
            assert!(x1 < x2 && y1 < y2, "invalid clip: {x1} {y1} {x2} {y2}");
            assert!(x1 >= 0 && y1 >= 0 && x2 <= buffer_size.0 && y2 <= buffer_size.1);
        }
    }
}

#[test]
fn damage_tracker_reproduces_empty_clips_and_wrapper_prevents_them() {
    for scale in SCALES {
        let mut raw_tracker = OutputDamageTracker::new(OUTPUT_SIZE, scale, Transform::Normal);
        let mut safe_tracker = OutputDamageTracker::new(OUTPUT_SIZE, scale, Transform::Normal);
        let mut elements = scene();
        let mut saw_zero_width = false;
        let mut saw_zero_height = false;
        let mut saw_invalid_clip = false;
        for frame in 0..5 {
            if frame == 1 {
                elements[0].commit.increment();
            }
            // Move without a content commit: both the old and new position
            // must still be damaged. The new position crosses the output at
            // fractional scales, exercising OutputDamageTracker's clipping.
            if frame == 2 {
                elements[1].geometry.loc = (420.0, 340.0).into();
            }
            // A formerly visible element becomes empty, then disappears.
            if frame == 3 {
                elements[1].geometry.size.w = 0.0;
            }
            if frame == 4 {
                elements.remove(1);
            }

            let raw = raw_tracker
                .damage_output(1, &elements)
                .unwrap()
                .0
                .cloned()
                .unwrap_or_default();
            saw_zero_width |= raw.iter().any(|r| r.size.w == 0);
            saw_zero_height |= raw.iter().any(|r| r.size.h == 0);
            let output = Rectangle::from_size(OUTPUT_SIZE.into());
            let src = Rectangle::from_size(Size::<f64, Buffer>::from((800.0, 600.0)));
            saw_invalid_clip |= plane_damage_clips(src, output, &raw)
                .iter()
                .any(|&[x1, y1, x2, y2]| x1 >= x2 || y1 >= y2);

            let wrapped: Vec<_> = elements
                .iter()
                .filter_map(|e| SanitizedElement::new(e, scale.into()))
                .collect();
            assert_eq!(wrapped.len(), if frame < 3 { 2 } else { 1 });
            let safe = safe_tracker
                .damage_output(1, &wrapped)
                .unwrap()
                .0
                .cloned()
                .unwrap_or_default();
            assert_valid_damage(&safe);
            if frame == 1 {
                assert!(
                    !safe.is_empty(),
                    "valid incremental damage was lost at scale {scale}"
                );
                assert_ne!(
                    safe,
                    vec![output],
                    "incremental damage became a full redraw"
                );
            }
            if frame == 2 || frame == 3 {
                let old_location = if frame == 2 {
                    (360.0, 280.0)
                } else {
                    (420.0, 340.0)
                };
                let old = rect(old_location.0, old_location.1, 80.0, 64.0)
                    .to_physical(scale)
                    .to_i32_round()
                    .intersection(output)
                    .unwrap();
                assert!(
                    Rectangle::subtract_rects_many_in_place(vec![old], safe.iter().copied())
                        .is_empty(),
                    "old position was not repainted"
                );
                if frame == 2 {
                    let new = elements[1]
                        .geometry(scale.into())
                        .intersection(output)
                        .unwrap();
                    assert!(
                        Rectangle::subtract_rects_many_in_place(vec![new], safe.iter().copied())
                            .is_empty(),
                        "new position was not repainted"
                    );
                }
            }
            if frame == 4 {
                assert!(safe.is_empty(), "unchanged scene should have no damage");
            }
        }
        assert!(
            saw_zero_width,
            "raw tracker did not reproduce zero-width damage at scale {scale}"
        );
        assert!(
            saw_zero_height,
            "raw tracker did not reproduce zero-height damage at scale {scale}"
        );
        assert!(
            saw_invalid_clip,
            "raw tracker did not reproduce invalid DRM clips at scale {scale}"
        );
    }
}
