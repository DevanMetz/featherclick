//! Where a screen point sits in a Wayland compositor's layout.
//!
//! wlroots maps an unbound virtual pointer's absolute motion across the
//! bounding box of every output: `position = origin + (value / extent) * size`.
//! enigo instead passes the first output's size as that extent, so a second
//! monitor, or one that sits left of the origin, is not the coordinate the
//! caller asked for. The extent has to be the whole layout, and the value has
//! to address the centre of the requested pixel. The centre survives the
//! compositor truncating or rounding that product onto a pixel.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

/// One output in the global compositor space. `width` and `height` are logical
/// pixels, the same space the pointer moves in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Output {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// What the compositor has told us about an output so far. Logical geometry
/// wins; a mode size is only a fallback for the single-output case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportedOutput {
    pub logical_position: Option<(i32, i32)>,
    pub logical_size: Option<(i32, i32)>,
    pub mode_size: Option<(i32, i32)>,
    pub scale: i32,
    /// `_90` and `_270` transforms swap the mode axes. Logical size is already
    /// swapped when the compositor sends it.
    pub transform_swaps_axes: bool,
}

/// Arguments for `zwlr_virtual_pointer_v1.motion_absolute`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbsoluteMotion {
    pub x: u32,
    pub y: u32,
    pub x_extent: u32,
    pub y_extent: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutError {
    NoDisplays,
    /// More than one output, and at least one has no logical position, so the
    /// layout would stack them on top of each other.
    PositionsUnknown,
    NotOnDisplay {
        x: i32,
        y: i32,
    },
    Extent,
}

impl LayoutError {
    pub fn message(&self) -> String {
        match self {
            Self::NoDisplays => "no displays".to_string(),
            Self::PositionsUnknown => {
                "the compositor did not report where each display sits".to_string()
            }
            Self::NotOnDisplay { x, y } => format!("point {x},{y} is not on a display"),
            Self::Extent => "the display layout is too large to address".to_string(),
        }
    }
}

impl std::fmt::Display for LayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

/// Builds the layout the pointer actually moves in.
pub fn layout_from(reported: &[ReportedOutput]) -> Result<Vec<Output>, LayoutError> {
    let sized: Vec<SizedOutput> = reported.iter().filter_map(ReportedOutput::sized).collect();
    if sized.is_empty() {
        return Err(LayoutError::NoDisplays);
    }
    let missing_position = sized.iter().any(|output| output.position.is_none());
    if missing_position && sized.len() > 1 {
        return Err(LayoutError::PositionsUnknown);
    }
    Ok(sized
        .into_iter()
        .map(|output| {
            let (x, y) = output.position.unwrap_or((0, 0));
            Output {
                x,
                y,
                width: output.width,
                height: output.height,
            }
        })
        .collect())
}

struct SizedOutput {
    position: Option<(i32, i32)>,
    width: i32,
    height: i32,
}

impl ReportedOutput {
    fn sized(&self) -> Option<SizedOutput> {
        let (width, height) = self.size()?;
        Some(SizedOutput {
            position: self.logical_position,
            width,
            height,
        })
    }

    fn size(&self) -> Option<(i32, i32)> {
        if let Some((width, height)) = self.logical_size {
            return positive(width, height);
        }
        let (mut width, mut height) = self.mode_size?;
        if self.transform_swaps_axes {
            std::mem::swap(&mut width, &mut height);
        }
        let scale = self.scale.max(1);
        positive(div_nearest(width, scale), div_nearest(height, scale))
    }
}

fn positive(width: i32, height: i32) -> Option<(i32, i32)> {
    (width > 0 && height > 0).then_some((width, height))
}

fn div_nearest(value: i32, scale: i32) -> i32 {
    let scale = scale.max(1);
    if value >= 0 {
        (value + scale / 2) / scale
    } else {
        (value - scale / 2) / scale
    }
}

/// Maps a global point onto one absolute-motion request for the whole layout.
///
/// A point on a shared edge belongs to the output on the right or below: the
/// rectangles are half-open. A point in a gap is rejected rather than pushed
/// onto a neighbour.
pub fn absolute_motion(outputs: &[Output], x: i32, y: i32) -> Result<AbsoluteMotion, LayoutError> {
    if outputs.is_empty() {
        return Err(LayoutError::NoDisplays);
    }
    if !outputs.iter().any(|output| contains(output, x, y)) {
        return Err(LayoutError::NotOnDisplay { x, y });
    }

    let origin_x = outputs.iter().map(|output| output.x).min().unwrap_or(0);
    let origin_y = outputs.iter().map(|output| output.y).min().unwrap_or(0);
    let edge_x = outputs
        .iter()
        .map(|output| output.x.saturating_add(output.width))
        .max()
        .unwrap_or(origin_x);
    let edge_y = outputs
        .iter()
        .map(|output| output.y.saturating_add(output.height))
        .max()
        .unwrap_or(origin_y);
    let width = edge_x.saturating_sub(origin_x);
    let height = edge_y.saturating_sub(origin_y);
    if width <= 0 || height <= 0 {
        return Err(LayoutError::NoDisplays);
    }

    let local_x = x.saturating_sub(origin_x);
    let local_y = y.saturating_sub(origin_y);
    let (x, x_extent) = centre_axis(local_x, width)?;
    let (y, y_extent) = centre_axis(local_y, height)?;
    Ok(AbsoluteMotion {
        x,
        y,
        x_extent,
        y_extent,
    })
}

fn contains(output: &Output, x: i32, y: i32) -> bool {
    x >= output.x
        && y >= output.y
        && x < output.x.saturating_add(output.width)
        && y < output.y.saturating_add(output.height)
}

/// `extent = size * 2` and `value = local * 2 + 1`, so the fraction is the
/// centre of the pixel: `(local + 0.5) / size`.
fn centre_axis(local: i32, size: i32) -> Result<(u32, u32), LayoutError> {
    let local = u32::try_from(local).map_err(|_| LayoutError::Extent)?;
    let size = u32::try_from(size).map_err(|_| LayoutError::Extent)?;
    if local >= size {
        return Err(LayoutError::Extent);
    }
    let extent = size.checked_mul(2).ok_or(LayoutError::Extent)?;
    let value = local.checked_mul(2).and_then(|value| value.checked_add(1));
    Ok((value.ok_or(LayoutError::Extent)?, extent))
}

/// Where wlroots puts the pointer for this request, in the layout's space.
#[cfg(test)]
fn compositor_position(origin: i32, size: i32, value: u32, extent: u32) -> f64 {
    origin as f64 + (value as f64 / extent as f64) * size as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(x: i32, y: i32, width: i32, height: i32) -> Output {
        Output {
            x,
            y,
            width,
            height,
        }
    }

    fn lands_on(origin: i32, size: i32, value: u32, extent: u32, pixel: i32) -> bool {
        let got = compositor_position(origin, size, value, extent);
        (got - (pixel as f64 + 0.5)).abs() < 1e-6
    }

    #[test]
    fn logical_geometry_beats_the_mode_and_a_lone_output_defaults_to_the_origin() {
        let logical = layout_from(&[ReportedOutput {
            logical_position: Some((10, 20)),
            logical_size: Some((800, 600)),
            mode_size: Some((3840, 2160)),
            scale: 2,
            transform_swaps_axes: true,
        }])
        .unwrap();
        assert_eq!(logical, vec![output(10, 20, 800, 600)]);

        let fallback = layout_from(&[ReportedOutput {
            logical_position: None,
            logical_size: None,
            mode_size: Some((3840, 2160)),
            scale: 2,
            transform_swaps_axes: false,
        }])
        .unwrap();
        assert_eq!(
            fallback,
            vec![output(0, 0, 1920, 1080)],
            "a single scaled mode is the logical size"
        );

        let turned = layout_from(&[ReportedOutput {
            logical_position: None,
            logical_size: None,
            mode_size: Some((1920, 1080)),
            scale: 1,
            transform_swaps_axes: true,
        }])
        .unwrap();
        assert_eq!(turned, vec![output(0, 0, 1080, 1920)]);
    }

    #[test]
    fn several_outputs_need_positions_and_empty_ones_are_dropped() {
        let missing = layout_from(&[
            ReportedOutput {
                logical_position: Some((0, 0)),
                logical_size: Some((100, 100)),
                mode_size: None,
                scale: 1,
                transform_swaps_axes: false,
            },
            ReportedOutput {
                logical_position: None,
                logical_size: Some((100, 100)),
                mode_size: None,
                scale: 1,
                transform_swaps_axes: false,
            },
        ]);
        assert_eq!(missing, Err(LayoutError::PositionsUnknown));

        let dropped = layout_from(&[
            ReportedOutput {
                logical_position: Some((0, 0)),
                logical_size: Some((0, 10)),
                mode_size: Some((50, 40)),
                scale: 1,
                transform_swaps_axes: false,
            },
            ReportedOutput {
                logical_position: None,
                logical_size: None,
                mode_size: None,
                scale: 1,
                transform_swaps_axes: false,
            },
        ]);
        assert_eq!(dropped, Err(LayoutError::NoDisplays));
    }

    #[test]
    fn a_point_lands_on_the_pixel_that_was_asked_for() {
        let outputs = vec![output(0, 0, 1920, 1080), output(1920, 0, 1920, 1080)];
        for (x, y) in [(0, 0), (1, 2), (1919, 1079), (1920, 0), (3839, 500)] {
            let motion = absolute_motion(&outputs, x, y).unwrap();
            assert!(
                lands_on(0, 3840, motion.x, motion.x_extent, x),
                "{x} landed at {}",
                compositor_position(0, 3840, motion.x, motion.x_extent)
            );
            assert!(
                lands_on(0, 1080, motion.y, motion.y_extent, y),
                "{y} landed at {}",
                compositor_position(0, 1080, motion.y, motion.y_extent)
            );
        }

        // A monitor left of the origin. The layout box starts at -1920.
        let shifted = vec![output(-1920, -100, 1920, 1080), output(0, -100, 1280, 800)];
        let motion = absolute_motion(&shifted, -1, -100).unwrap();
        assert!(lands_on(-1920, 3200, motion.x, motion.x_extent, -1));
        assert!(lands_on(-100, 1080, motion.y, motion.y_extent, -100));
        let motion = absolute_motion(&shifted, 1279, 699).unwrap();
        assert!(lands_on(-1920, 3200, motion.x, motion.x_extent, 1279));
    }

    #[test]
    fn gaps_and_the_far_side_of_an_edge_are_not_a_neighbouring_pixel() {
        // L shape: nothing at (150, 10).
        let shape = vec![output(0, 0, 100, 100), output(0, 100, 200, 50)];
        assert_eq!(
            absolute_motion(&shape, 150, 10),
            Err(LayoutError::NotOnDisplay { x: 150, y: 10 })
        );
        // The shared edge x = 100 belongs to the output that starts there,
        // which this shape does not have on the top row, so it is outside.
        assert!(matches!(
            absolute_motion(&shape, 100, 0),
            Err(LayoutError::NotOnDisplay { .. })
        ));
        assert!(absolute_motion(&shape, 99, 99).is_ok());
        assert!(absolute_motion(&shape, 100, 100).is_ok());
        assert_eq!(absolute_motion(&[], 0, 0), Err(LayoutError::NoDisplays));
    }
}
