use tao::window::{CursorIcon, ResizeDirection};

pub(crate) fn resize_direction_for_point(
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    border: f64,
) -> Option<ResizeDirection> {
    let top = y <= border;
    let bottom = y >= height - border;
    let left = x <= border;
    let right = x >= width - border;

    if top && left {
        Some(ResizeDirection::NorthWest)
    } else if top && right {
        Some(ResizeDirection::NorthEast)
    } else if bottom && left {
        Some(ResizeDirection::SouthWest)
    } else if bottom && right {
        Some(ResizeDirection::SouthEast)
    } else if top {
        Some(ResizeDirection::North)
    } else if bottom {
        Some(ResizeDirection::South)
    } else if left {
        Some(ResizeDirection::West)
    } else if right {
        Some(ResizeDirection::East)
    } else {
        None
    }
}

pub(crate) fn is_near_resize_edge(x: f64, y: f64, width: f64, height: f64, hot_zone: f64) -> bool {
    x <= hot_zone || x >= width - hot_zone || y <= hot_zone || y >= height - hot_zone
}

pub(crate) fn cursor_icon_for_resize_direction(direction: ResizeDirection) -> CursorIcon {
    match direction {
        ResizeDirection::North => CursorIcon::NResize,
        ResizeDirection::South => CursorIcon::SResize,
        ResizeDirection::East => CursorIcon::EResize,
        ResizeDirection::West => CursorIcon::WResize,
        ResizeDirection::NorthEast => CursorIcon::NeResize,
        ResizeDirection::NorthWest => CursorIcon::NwResize,
        ResizeDirection::SouthEast => CursorIcon::SeResize,
        ResizeDirection::SouthWest => CursorIcon::SwResize,
    }
}
