//! Safe-area insets that the iOS shell reports to the page. Pure, so desktop
//! unit tests cover the iPhone Duo cases.
#![cfg_attr(not(target_os = "ios"), allow(dead_code))]

/// Insets in points (CSS px), in web view coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Insets {
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
    pub left: f64,
}

/// UIKit geometry that selects the reported insets.
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub safe: Insets,
    /// Window width and height, in points.
    pub window: (f64, f64),
    /// statusBarManager.statusBarFrame height, in points.
    pub status_bar_height: Option<f64>,
    /// UIInterfaceOrientation raw value.
    pub orientation: isize,
}

/// Smallest UIKit top inset, in points, that is the misreported Duo side
/// strip and not a status bar (the 27.2 strip reads 82; a status bar 20 to 59).
pub const MISREPORTED_STRIP_MIN: f64 = 64.0;
/// Largest status bar frame height, in points, of the iOS 27.2 Duo case,
/// where the status bar is a hairline at the top and the strip is at the side.
pub const HAIRLINE_STATUS_BAR_MAX: f64 = 2.0;
const LANDSCAPE_RIGHT: isize = 3;
const LANDSCAPE_LEFT: isize = 4;

/// iOS 27.2 on the iPhone Duo (open pose, landscape) reports the side status
/// strip as a top inset. Move it to the side of the device top edge:
/// landscapeLeft has that edge on the right, landscapeRight on the left.
/// Every other geometry passes UIKit's values unchanged.
pub fn reported(geometry: &Geometry) -> Insets {
    let Geometry {
        safe,
        window: (width, height),
        status_bar_height,
        orientation,
    } = *geometry;
    let misreported = width > height
        && safe.top >= MISREPORTED_STRIP_MIN
        && safe.left == 0.0
        && safe.right == 0.0
        && status_bar_height.is_some_and(|h| h <= HAIRLINE_STATUS_BAR_MAX);
    match (misreported, orientation) {
        (true, LANDSCAPE_LEFT) => Insets {
            top: 0.0,
            right: safe.top,
            ..safe
        },
        (true, LANDSCAPE_RIGHT) => Insets {
            top: 0.0,
            left: safe.top,
            ..safe
        },
        _ => safe,
    }
}

/// A rectangle in web view coordinates (CSS px).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// The system status column (clock, Wi-Fi, call and recording pills): the
/// union of the active occlusion regions (iOS 27.1 UIViewReservedRegion)
/// that overlap a reported top, left or right strip. Its size follows the
/// status content, so a call pill makes it longer. None without such a region.
pub fn status_column(insets: &Insets, width: f64, occlusions: &[Rect]) -> Option<Rect> {
    occlusions
        .iter()
        .filter(|r| r.width > 0.0 && r.height > 0.0)
        .filter(|r| {
            (insets.top > 0.0 && r.y < insets.top)
                || (insets.left > 0.0 && r.x < insets.left)
                || (insets.right > 0.0 && r.x + r.width > width - insets.right)
        })
        .copied()
        .reduce(|a, b| {
            let x = a.x.min(b.x);
            let y = a.y.min(b.y);
            Rect {
                x,
                y,
                width: (a.x + a.width).max(b.x + b.width) - x,
                height: (a.y + a.height).max(b.y + b.height) - y,
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORTRAIT: isize = 1;

    fn insets(top: f64, right: f64, bottom: f64, left: f64) -> Insets {
        Insets {
            top,
            right,
            bottom,
            left,
        }
    }

    fn duo_272(orientation: isize) -> Geometry {
        Geometry {
            safe: insets(82.0, 0.0, 34.0, 0.0),
            window: (951.0, 669.0),
            status_bar_height: Some(2.0),
            orientation,
        }
    }

    #[test]
    fn landscape_left_moves_the_misreported_strip_to_the_right() {
        assert_eq!(
            reported(&duo_272(LANDSCAPE_LEFT)),
            insets(0.0, 82.0, 34.0, 0.0)
        );
    }

    #[test]
    fn landscape_right_moves_the_misreported_strip_to_the_left() {
        assert_eq!(
            reported(&duo_272(LANDSCAPE_RIGHT)),
            insets(0.0, 0.0, 34.0, 82.0)
        );
    }

    #[test]
    fn portrait_keeps_uikit_values() {
        let geometry = Geometry {
            safe: insets(82.0, 0.0, 34.0, 0.0),
            window: (466.0, 678.0),
            status_bar_height: Some(2.0),
            orientation: PORTRAIT,
        };
        assert_eq!(reported(&geometry), geometry.safe);
    }

    #[test]
    fn ios_27_1_side_strip_is_reported_directly() {
        let geometry = Geometry {
            safe: insets(0.0, 84.0, 34.0, 0.0),
            window: (951.0, 669.0),
            status_bar_height: Some(0.0),
            orientation: LANDSCAPE_LEFT,
        };
        assert_eq!(reported(&geometry), geometry.safe);
    }

    #[test]
    fn a_real_top_status_bar_or_a_short_top_inset_is_unchanged() {
        let mut status_bar = duo_272(LANDSCAPE_LEFT);
        status_bar.status_bar_height = Some(24.0);
        assert_eq!(reported(&status_bar), status_bar.safe);
        let mut unknown = duo_272(LANDSCAPE_LEFT);
        unknown.status_bar_height = None;
        assert_eq!(reported(&unknown), unknown.safe);
        let mut short = duo_272(LANDSCAPE_LEFT);
        short.safe.top = 59.0;
        assert_eq!(reported(&short), short.safe);
    }

    fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn status_column_is_the_active_occlusion_in_the_side_strip() {
        // Duo simulator, iOS 27.2, landscapeLeft: the status column region
        // in the right strip; a region in the page area does not count.
        let strip = insets(0.0, 82.0, 34.0, 0.0);
        let column = rect(867.0, 0.0, 84.0, 124.0);
        assert_eq!(
            status_column(&strip, 951.0, &[rect(677.3, 21.0, 58.0, 37.0), column]),
            Some(column)
        );
        let left = insets(0.0, 0.0, 34.0, 82.0);
        assert_eq!(
            status_column(&left, 951.0, &[rect(0.0, 0.0, 84.0, 124.0)]),
            Some(rect(0.0, 0.0, 84.0, 124.0))
        );
        assert_eq!(status_column(&strip, 951.0, &[]), None);
    }

    #[test]
    fn a_call_pill_region_extends_the_status_column() {
        let strip = insets(0.0, 82.0, 34.0, 0.0);
        assert_eq!(
            status_column(
                &strip,
                951.0,
                &[
                    rect(867.0, 0.0, 84.0, 124.0),
                    rect(871.0, 120.0, 76.0, 70.0)
                ]
            ),
            Some(rect(867.0, 0.0, 84.0, 190.0))
        );
    }

    #[test]
    fn status_column_in_the_portrait_top_strip() {
        let top = insets(82.0, 0.0, 34.0, 0.0);
        assert_eq!(
            status_column(&top, 669.0, &[rect(540.0, 0.0, 129.0, 82.0)]),
            Some(rect(540.0, 0.0, 129.0, 82.0))
        );
        // No strip: nothing is a status column.
        let none = insets(0.0, 0.0, 34.0, 0.0);
        assert_eq!(
            status_column(&none, 669.0, &[rect(540.0, 0.0, 129.0, 82.0)]),
            None
        );
    }
}
