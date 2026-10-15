//! Surfaces: what clients draw. A surface's state changes are *pending*
//! until the client commits them; then they apply at once (double
//! buffering), so the screen never shows half an update. A surface takes
//! a *role*: a window (xdg_toplevel), a popup menu (xdg_popup), part of
//! another surface (a subsurface), or the mouse pointer.
//!
//! Committed buffer contents are copied into the surface and the buffer
//! is released at once, so clients can reuse it immediately and the
//! desktop never reads memory a client is changing.

use alloc::string::String;
use alloc::vec::Vec;

use crate::render::Rect;

#[derive(Default)]
pub struct Pending {
    /// `attach` was called: with a buffer (its object id), or with none
    /// (unmapping).
    pub buffer: Option<Option<u32>>,
    pub offset: (i32, i32),
    /// Damaged area, in surface coordinates (bounding box).
    pub damage: Option<Rect>,
    pub frame_callbacks: Vec<u32>,
    pub scale: Option<i32>,
    pub input_region: Option<Option<Vec<Rect>>>,
}

impl Pending {
    pub fn add_damage(&mut self, rect: Rect) {
        self.damage = Some(match self.damage {
            Some(damage) => damage.union(&rect),
            None => rect,
        });
    }

    /// Folds later pending state into this one (a synchronized
    /// subsurface's commits accumulate until its parent commits).
    pub fn merge(&mut self, later: Pending) {
        if later.buffer.is_some() {
            self.buffer = later.buffer;
        }
        self.offset.0 += later.offset.0;
        self.offset.1 += later.offset.1;
        if let Some(damage) = later.damage {
            self.add_damage(damage);
        }
        self.frame_callbacks.extend(later.frame_callbacks);
        if later.scale.is_some() {
            self.scale = later.scale;
        }
        if later.input_region.is_some() {
            self.input_region = later.input_region;
        }
    }
}

pub struct Toplevel {
    pub object: u32,
    pub title: String,
    pub app_id: String,
    /// A dialog's parent window (its surface).
    pub parent: Option<u32>,
    pub min_size: (i32, i32),
    pub max_size: (i32, i32),
    pub fullscreen: bool,
    /// Where its window geometry's top left is on screen.
    pub x: i32,
    pub y: i32,
    /// The size last configured (0: the client chooses).
    pub configured: (i32, i32),
    pub activated: bool,
}

pub struct Popup {
    pub object: u32,
    /// The parent's surface.
    pub parent: u32,
    /// Position relative to the parent's window geometry, and size.
    pub rect: Rect,
    pub grab: bool,
}

pub struct Subsurface {
    pub parent: u32,
    pub x: i32,
    pub y: i32,
    /// Position set, applied with the parent's next commit.
    pub pending_position: Option<(i32, i32)>,
    pub sync: bool,
}

pub enum Role {
    None,
    Toplevel(Toplevel),
    Popup(Popup),
    Subsurface(Subsurface),
    Cursor,
}

pub struct Xdg {
    pub object: u32,
    pub geometry: Option<Rect>,
    /// The initial commit was made, so it can be configured.
    pub initial_commit: bool,
    pub configured: bool,
}

pub struct Surface {
    pub pixels: Vec<u32>,
    /// Size in surface coordinates (the buffer's divided by its scale).
    pub width: i32,
    pub height: i32,
    pub buffer_width: i32,
    /// The buffer has an alpha channel.
    pub alpha: bool,
    pub scale: i32,
    pub has_content: bool,
    pub pending: Pending,
    /// A synchronized subsurface's state, waiting for its parent's commit.
    pub cached: Option<Pending>,
    /// Frame callbacks committed, to be done at the next frame.
    pub frame_callbacks: Vec<u32>,
    pub role: Role,
    pub xdg: Option<Xdg>,
    /// Subsurfaces, drawn above this one in order.
    pub children: Vec<u32>,
    pub input_region: Option<Vec<Rect>>,
}

impl Surface {
    pub fn new() -> Self {
        Self {
            pixels: Vec::new(),
            width: 0,
            height: 0,
            buffer_width: 0,
            alpha: false,
            scale: 1,
            has_content: false,
            pending: Pending::default(),
            cached: None,
            frame_callbacks: Vec::new(),
            role: Role::None,
            xdg: None,
            children: Vec::new(),
            input_region: None,
        }
    }

    /// The window geometry: the part that is the window proper (without
    /// client-drawn shadows), in surface coordinates.
    pub fn geometry(&self) -> Rect {
        self.xdg.as_ref().and_then(|xdg| xdg.geometry).filter(|geometry| !geometry.is_empty()).unwrap_or(Rect::new(
            0,
            0,
            self.width,
            self.height,
        ))
    }

    pub fn toplevel(&self) -> Option<&Toplevel> {
        match &self.role {
            Role::Toplevel(toplevel) => Some(toplevel),
            _ => None,
        }
    }

    pub fn toplevel_mut(&mut self) -> Option<&mut Toplevel> {
        match &mut self.role {
            Role::Toplevel(toplevel) => Some(toplevel),
            _ => None,
        }
    }

    /// Whether a point (surface coordinates) is in its input region.
    pub fn accepts_input(&self, x: i32, y: i32) -> bool {
        if !Rect::new(0, 0, self.width, self.height).contains(x, y) {
            return false;
        }
        match &self.input_region {
            None => true,
            Some(region) => region.iter().any(|rect| rect.contains(x, y)),
        }
    }
}
