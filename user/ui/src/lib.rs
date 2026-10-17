//! ui: eliOS's toolkit for the desktop and the programs that run in it.
//!
//! - [`wire`]: the Wayland wire format, used by both ends.
//! - [`window`]: a client: a window to draw in, and its input.
//! - [`draw`]: rectangles, anti-aliased shapes and TrueType text.
//! - [`widgets`]: buttons, text fields, lists, menus, icons.
//! - [`keys`]: what key codes mean.
//! - [`sys`]: the system calls all this needs beyond libeli's.

#![no_std]

extern crate alloc;

pub mod draw;
pub mod keys;
pub mod sys;
pub mod widgets;
pub mod window;
pub mod wire;

pub use draw::{Canvas, Font, Rect};
pub use keys::{Key, Modifiers};
pub use window::{Button, Event, Window};
