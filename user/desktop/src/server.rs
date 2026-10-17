//! The Wayland server: clients, their objects, and the requests they send.
//!
//! Every client has a table of objects by id (ids are chosen by the
//! client). Requests are dispatched on the object's kind; most change
//! surface state (see `surface.rs`), which the shell (`shell.rs`) lays out
//! and draws.
//!
//! Globals offered: wl_compositor, wl_subcompositor, wl_shm, wl_seat,
//! wl_output, xdg_wm_base and wl_data_device_manager (a stub, enough for
//! toolkits that insist on one).

use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

use crate::render::Rect;
use crate::surface::{Popup, Role, Subsurface, Surface, Toplevel, Xdg};
use crate::sys;
use crate::wire::{Arguments, Broken, Connection, Message, Outgoing};

/// A client's surface, as (client, surface object id).
pub type SurfaceRef = (u32, u32);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Display,
    Registry,
    Callback,
    Compositor,
    Surface,
    Region,
    Shm,
    ShmPool,
    Buffer,
    Seat,
    Pointer,
    Keyboard,
    Touch,
    Output,
    Subcompositor,
    Subsurface(u32),
    WmBase,
    Positioner,
    XdgSurface(u32),
    Toplevel(u32),
    Popup(u32),
    DataDeviceManager,
    DataDevice,
    DataSource,
}

pub struct Object {
    pub kind: Kind,
    pub version: u32,
}

/// A client's shared memory, mapped.
pub struct Pool {
    address: *mut u8,
    size: usize,
}

impl Drop for Pool {
    fn drop(&mut self) {
        // SAFETY: nothing refers to the mapping once the pool and its
        // buffers are gone.
        unsafe { sys::unmap(self.address, self.size) };
    }
}

pub struct Buffer {
    pool: Rc<Pool>,
    offset: usize,
    pub width: i32,
    pub height: i32,
    stride: usize,
    format: u32,
}

const FORMAT_ARGB8888: u32 = 0;
const FORMAT_XRGB8888: u32 = 1;

impl Buffer {
    /// Copies the buffer's pixels into `pixels`, a row at a time. If
    /// `pixels` already holds a picture this size, only `damage` (buffer
    /// coordinates) is copied: Wayland lets the compositor assume the rest
    /// is unchanged. Returns the part copied, or `None` if the buffer
    /// doesn't fit its pool (which a client could arrange by shrinking the
    /// file).
    fn copy_into(&self, pixels: &mut Vec<u32>, damage: Option<Rect>) -> Option<Rect> {
        let (width, height) = (self.width.max(0) as usize, self.height.max(0) as usize);
        let bytes = self.stride.checked_mul(height)?;
        if self.offset.checked_add(bytes)? > self.pool.size || self.stride < width * 4 {
            return None;
        }
        let whole = Rect::new(0, 0, self.width, self.height);
        let area = match damage {
            Some(damage) if pixels.len() == width * height => damage.intersect(&whole),
            _ => {
                pixels.clear();
                pixels.resize(width * height, 0);
                whole
            }
        };
        for row in area.y..area.y + area.height {
            let start = row as usize * width + area.x as usize;
            let target = &mut pixels[start..start + area.width as usize];
            // SAFETY: the rows lie within the mapping (checked above), and
            // `target` is as long as the run copied. The client may be
            // writing to the buffer, which can only garble its own picture.
            unsafe {
                let source = self.pool.address.add(self.offset + row as usize * self.stride + area.x as usize * 4);
                core::ptr::copy_nonoverlapping(source, target.as_mut_ptr().cast::<u8>(), target.len() * 4);
            }
        }
        Some(area)
    }
}

pub struct Positioner {
    pub size: (i32, i32),
    pub anchor_rect: Rect,
    pub anchor: u32,
    pub gravity: u32,
    pub adjustment: u32,
    pub offset: (i32, i32),
}

impl Positioner {
    fn new() -> Self {
        Self { size: (1, 1), anchor_rect: Rect::new(0, 0, 1, 1), anchor: 0, gravity: 0, adjustment: 0, offset: (0, 0) }
    }
}

pub struct Client {
    pub connection: Connection,
    pub objects: BTreeMap<u32, Object>,
    pub surfaces: BTreeMap<u32, Surface>,
    /// Each pool's current mapping, and its file (kept for resizing).
    pools: BTreeMap<u32, (Rc<Pool>, i32)>,
    buffers: BTreeMap<u32, Buffer>,
    regions: BTreeMap<u32, Vec<Rect>>,
    positioners: BTreeMap<u32, Positioner>,
    /// What the last commit changed, for the screen to redraw: each
    /// surface applied and the part of it that changed (surface
    /// coordinates; `None` for all of it).
    pub applied: Vec<(u32, Option<Rect>)>,
    /// A subsurface moved: its old place needs redrawing too.
    pub moved: bool,
}

impl Client {
    pub fn new(fd: i32) -> Self {
        let mut objects = BTreeMap::new();
        objects.insert(1, Object { kind: Kind::Display, version: 1 });
        Self {
            connection: Connection::new(fd),
            objects,
            surfaces: BTreeMap::new(),
            pools: BTreeMap::new(),
            buffers: BTreeMap::new(),
            regions: BTreeMap::new(),
            positioners: BTreeMap::new(),
            applied: Vec::new(),
            moved: false,
        }
    }

    pub fn send(&mut self, event: Outgoing) {
        self.connection.send(event);
    }

    /// Objects of one kind, with their versions.
    pub fn objects_of(&self, kind: Kind) -> Vec<(u32, u32)> {
        self.objects.iter().filter(|(_, object)| object.kind == kind).map(|(&id, o)| (id, o.version)).collect()
    }

    fn add(&mut self, id: u32, kind: Kind, version: u32) -> Result<(), Broken> {
        if id == 0 || self.objects.contains_key(&id) {
            return Err(Broken::Protocol("object id already in use"));
        }
        self.objects.insert(id, Object { kind, version });
        Ok(())
    }

    /// Closes the files of pools still open (the client went away).
    pub fn close_pools(&mut self) {
        for (_, (_, fd)) in core::mem::take(&mut self.pools) {
            let _ = eli::io::close(fd);
        }
    }

    /// Forgets an object the client destroyed, telling it the id is free.
    pub fn remove(&mut self, id: u32) {
        if self.objects.remove(&id).is_some() && id < 0xFF00_0000 {
            self.send(Outgoing::new(1, 1).uint(id));
        }
    }
}

/// (global name, interface, version) for each global offered.
const GLOBALS: [(u32, &str, u32); 7] = [
    (1, "wl_compositor", 5),
    (2, "wl_subcompositor", 1),
    (3, "wl_shm", 1),
    (4, "wl_seat", 7),
    (5, "wl_output", 3),
    (6, "xdg_wm_base", 2),
    (7, "wl_data_device_manager", 3),
];

/// What a request asks the shell to do.
pub enum Action {
    None,
    /// A surface's committed state changed (content or role).
    Committed(u32),
    /// A toplevel was created and should be configured.
    NewToplevel(u32),
    /// A popup was created; position it.
    NewPopup(u32),
    /// A toplevel changed what it wants (title, fullscreen...).
    ToplevelChanged(u32),
    SurfaceDestroyed(u32),
    /// The client set the pointer image.
    SetCursor(Option<u32>, i32, i32),
    /// A popup asked for an input grab.
    PopupGrab(u32),
    /// New keyboard: send the keymap and focus state.
    NewKeyboard(u32),
    /// New output: describe the screen.
    NewOutput(u32, u32),
    NewPointer,
}

impl Client {
    /// Handles one request.
    pub fn dispatch(&mut self, message: Message, serial: &mut u32) -> Result<Action, Broken> {
        let Some(object) = self.objects.get(&message.object) else {
            // Requests to objects destroyed meanwhile are ignored.
            return Ok(Action::None);
        };
        let (kind, version) = (object.kind, object.version);
        let mut args = Arguments::new(&message.arguments);
        let id = message.object;
        let opcode = message.opcode;
        match kind {
            Kind::Display => match opcode {
                // sync
                0 => {
                    let callback = args.uint()?;
                    self.send(Outgoing::new(callback, 0).uint(*serial));
                    self.send(Outgoing::new(1, 1).uint(callback));
                }
                // get_registry
                1 => {
                    let registry = args.uint()?;
                    self.add(registry, Kind::Registry, 1)?;
                    for (name, interface, version) in GLOBALS {
                        self.send(Outgoing::new(registry, 0).uint(name).string(interface).uint(version));
                    }
                }
                _ => return Err(Broken::Protocol("bad wl_display request")),
            },
            Kind::Registry => {
                // bind(name, interface, version, id)
                let name = args.uint()?;
                let interface = args.string()?.unwrap_or_default();
                let version = args.uint()?;
                let new = args.uint()?;
                let Some(&(_, global, max)) = GLOBALS.iter().find(|(n, _, _)| *n == name) else {
                    return Err(Broken::Protocol("no such global"));
                };
                if global != interface || version == 0 || version > max {
                    return Err(Broken::Protocol("bad global binding"));
                }
                return self.bind(global, new, version);
            }
            Kind::Compositor => match opcode {
                0 => {
                    let new = args.uint()?;
                    self.add(new, Kind::Surface, version)?;
                    self.surfaces.insert(new, Surface::new());
                }
                1 => {
                    let new = args.uint()?;
                    self.add(new, Kind::Region, version)?;
                    self.regions.insert(new, Vec::new());
                }
                _ => self.remove(id),
            },
            Kind::Region => match opcode {
                0 => {
                    self.regions.remove(&id);
                    self.remove(id);
                }
                1 | 2 => {
                    let rect = Rect::new(args.int()?, args.int()?, args.int()?, args.int()?);
                    let region = self.regions.entry(id).or_default();
                    // Subtracting is rare (and approximated by ignoring it).
                    if opcode == 1 {
                        region.push(rect);
                    }
                }
                _ => {}
            },
            Kind::Surface => return self.surface_request(id, opcode, &mut args),
            Kind::Shm => match opcode {
                // create_pool(id, fd, size)
                0 => {
                    let new = args.uint()?;
                    let fd = self.connection.take_fd().ok_or(Broken::Protocol("create_pool without a file"))?;
                    let size = args.int()?;
                    let Ok(address) = sys::map(fd, size.max(1) as usize, false) else {
                        let _ = eli::io::close(fd);
                        return Err(Broken::Protocol("can't map the pool"));
                    };
                    self.add(new, Kind::ShmPool, version)?;
                    self.pools.insert(new, (Rc::new(Pool { address, size: size as usize }), fd));
                }
                _ => self.remove(id),
            },
            Kind::ShmPool => match opcode {
                // create_buffer(id, offset, width, height, stride, format)
                0 => {
                    let new = args.uint()?;
                    let (offset, width, height, stride, format) =
                        (args.int()?, args.int()?, args.int()?, args.int()?, args.uint()?);
                    if width <= 0 || height <= 0 || stride < width * 4 || offset < 0 {
                        return Err(Broken::Protocol("bad buffer size"));
                    }
                    if format != FORMAT_ARGB8888 && format != FORMAT_XRGB8888 {
                        return Err(Broken::Protocol("unsupported buffer format"));
                    }
                    let pool = self.pools.get(&id).ok_or(Broken::Protocol("no such pool"))?.0.clone();
                    self.add(new, Kind::Buffer, 1)?;
                    self.buffers.insert(
                        new,
                        Buffer { pool, offset: offset as usize, width, height, stride: stride as usize, format },
                    );
                }
                // destroy: buffers made from it keep the memory mapped.
                1 => {
                    if let Some((_, fd)) = self.pools.remove(&id) {
                        let _ = eli::io::close(fd);
                    }
                    self.remove(id);
                }
                // resize: the file grew. Map it again; buffers already
                // made keep the old mapping, which still covers them.
                2 => {
                    let size = args.int()?.max(1) as usize;
                    if let Some((pool, fd)) = self.pools.get_mut(&id)
                        && size > pool.size
                    {
                        let address = sys::map(*fd, size, false).map_err(|_| Broken::Protocol("can't map the pool"))?;
                        *pool = Rc::new(Pool { address, size });
                    }
                }
                _ => {}
            },
            Kind::Buffer => {
                self.buffers.remove(&id);
                self.remove(id);
            }
            Kind::Seat => match opcode {
                0 => {
                    let new = args.uint()?;
                    self.add(new, Kind::Pointer, version)?;
                    return Ok(Action::NewPointer);
                }
                1 => {
                    let new = args.uint()?;
                    self.add(new, Kind::Keyboard, version)?;
                    return Ok(Action::NewKeyboard(new));
                }
                2 => {
                    let new = args.uint()?;
                    self.add(new, Kind::Touch, version)?;
                }
                _ => self.remove(id),
            },
            Kind::Pointer => match opcode {
                // set_cursor(serial, surface, hotspot x, y)
                0 => {
                    let _serial = args.uint()?;
                    let surface = args.uint()?;
                    let (x, y) = (args.int()?, args.int()?);
                    if surface == 0 {
                        return Ok(Action::SetCursor(None, 0, 0));
                    }
                    if let Some(cursor) = self.surfaces.get_mut(&surface)
                        && matches!(cursor.role, Role::None | Role::Cursor)
                    {
                        cursor.role = Role::Cursor;
                        return Ok(Action::SetCursor(Some(surface), x, y));
                    }
                    return Err(Broken::Protocol("cursor surface has another role"));
                }
                _ => self.remove(id),
            },
            Kind::Keyboard | Kind::Touch | Kind::Output | Kind::Callback => self.remove(id),
            Kind::Subcompositor => match opcode {
                0 => self.remove(id),
                // get_subsurface(id, surface, parent)
                _ => {
                    let (new, surface, parent) = (args.uint()?, args.uint()?, args.uint()?);
                    if surface == parent || !self.surfaces.contains_key(&parent) {
                        return Err(Broken::Protocol("bad subsurface parent"));
                    }
                    let child = self.surfaces.get_mut(&surface).ok_or(Broken::Protocol("no such surface"))?;
                    if !matches!(child.role, Role::None) {
                        return Err(Broken::Protocol("surface already has a role"));
                    }
                    child.role =
                        Role::Subsurface(Subsurface { parent, x: 0, y: 0, pending_position: None, sync: true });
                    self.add(new, Kind::Subsurface(surface), 1)?;
                    if let Some(parent) = self.surfaces.get_mut(&parent) {
                        parent.children.push(surface);
                    }
                }
            },
            Kind::Subsurface(surface) => return self.subsurface_request(id, surface, opcode, &mut args),
            Kind::WmBase => match opcode {
                0 => self.remove(id),
                1 => {
                    let new = args.uint()?;
                    self.add(new, Kind::Positioner, version)?;
                    self.positioners.insert(new, Positioner::new());
                }
                // get_xdg_surface(id, surface)
                2 => {
                    let (new, surface) = (args.uint()?, args.uint()?);
                    let target = self.surfaces.get_mut(&surface).ok_or(Broken::Protocol("no such surface"))?;
                    if target.xdg.is_some() || target.has_content {
                        return Err(Broken::Protocol("surface already has a role or a buffer"));
                    }
                    target.xdg = Some(Xdg { object: new, geometry: None, initial_commit: false, configured: false });
                    self.add(new, Kind::XdgSurface(surface), version)?;
                }
                // pong
                _ => {}
            },
            Kind::Positioner => {
                let positioner = self.positioners.entry(id).or_insert_with(Positioner::new);
                match opcode {
                    0 => {
                        self.positioners.remove(&id);
                        self.remove(id);
                    }
                    1 => positioner.size = (args.int()?, args.int()?),
                    2 => positioner.anchor_rect = Rect::new(args.int()?, args.int()?, args.int()?, args.int()?),
                    3 => positioner.anchor = args.uint()?,
                    4 => positioner.gravity = args.uint()?,
                    5 => positioner.adjustment = args.uint()?,
                    6 => positioner.offset = (args.int()?, args.int()?),
                    _ => {}
                }
            }
            Kind::XdgSurface(surface) => {
                return self.xdg_surface_request(id, surface, opcode, &mut args, version, serial);
            }
            Kind::Toplevel(surface) => return self.toplevel_request(id, surface, opcode, &mut args),
            Kind::Popup(surface) => match opcode {
                0 => {
                    if let Some(target) = self.surfaces.get_mut(&surface) {
                        target.role = Role::None;
                    }
                    self.remove(id);
                    return Ok(Action::SurfaceDestroyed(surface));
                }
                1 => return Ok(Action::PopupGrab(surface)),
                _ => {}
            },
            Kind::DataDeviceManager => match opcode {
                0 => {
                    let new = args.uint()?;
                    self.add(new, Kind::DataSource, version)?;
                }
                1 => {
                    let new = args.uint()?;
                    self.add(new, Kind::DataDevice, version)?;
                }
                _ => self.remove(id),
            },
            Kind::DataDevice => {
                if opcode == 2 {
                    self.remove(id);
                }
            }
            Kind::DataSource => {
                if opcode == 1 {
                    self.remove(id);
                } else if opcode == 0 {
                    // offer(mime type): nothing reads the clipboard yet.
                    let _ = args.string()?;
                }
            }
        }
        Ok(Action::None)
    }

    fn bind(&mut self, interface: &str, new: u32, version: u32) -> Result<Action, Broken> {
        let kind = match interface {
            "wl_compositor" => Kind::Compositor,
            "wl_subcompositor" => Kind::Subcompositor,
            "wl_shm" => Kind::Shm,
            "wl_seat" => Kind::Seat,
            "wl_output" => Kind::Output,
            "xdg_wm_base" => Kind::WmBase,
            _ => Kind::DataDeviceManager,
        };
        self.add(new, kind, version)?;
        match kind {
            Kind::Shm => {
                self.send(Outgoing::new(new, 0).uint(FORMAT_ARGB8888));
                self.send(Outgoing::new(new, 0).uint(FORMAT_XRGB8888));
            }
            Kind::Seat => {
                // Pointer and keyboard.
                self.send(Outgoing::new(new, 0).uint(3));
                if version >= 2 {
                    self.send(Outgoing::new(new, 1).string("seat0"));
                }
            }
            Kind::Output => return Ok(Action::NewOutput(new, version)),
            _ => {}
        }
        Ok(Action::None)
    }

    fn surface_request(&mut self, id: u32, opcode: u16, args: &mut Arguments) -> Result<Action, Broken> {
        let surface = self.surfaces.get_mut(&id).ok_or(Broken::Protocol("no such surface"))?;
        match opcode {
            0 => {
                self.surfaces.remove(&id);
                for other in self.surfaces.values_mut() {
                    other.children.retain(|&child| child != id);
                }
                self.remove(id);
                return Ok(Action::SurfaceDestroyed(id));
            }
            // attach(buffer, x, y)
            1 => {
                let buffer = args.uint()?;
                let (x, y) = (args.int()?, args.int()?);
                surface.pending.buffer = Some((buffer != 0).then_some(buffer));
                surface.pending.offset = (x, y);
            }
            // damage and damage_buffer (buffer coordinates are close
            // enough at scale 1, and over-damaging is harmless).
            2 | 9 => {
                let rect = Rect::new(args.int()?, args.int()?, args.int()?, args.int()?);
                let scale = surface.scale.max(1);
                let rect = if opcode == 9 {
                    Rect::new(rect.x / scale, rect.y / scale, rect.width / scale + 1, rect.height / scale + 1)
                } else {
                    rect
                };
                surface.pending.add_damage(rect);
            }
            3 => {
                let callback = args.uint()?;
                surface.pending.frame_callbacks.push(callback);
                self.add(callback, Kind::Callback, 1)?;
            }
            // set_opaque_region: only an optimization hint.
            4 => {}
            5 => {
                let region = args.uint()?;
                let rects =
                    if region == 0 { None } else { Some(self.regions.get(&region).cloned().unwrap_or_default()) };
                if let Some(surface) = self.surfaces.get_mut(&id) {
                    surface.pending.input_region = Some(rects);
                }
            }
            6 => return self.commit(id),
            // set_buffer_transform: only the normal transform is drawn.
            7 => {}
            8 => surface.pending.scale = Some(args.int()?.max(1)),
            // offset(x, y)
            10 => surface.pending.offset = (args.int()?, args.int()?),
            _ => {}
        }
        Ok(Action::None)
    }

    /// `commit`: applies the pending state, or for a synchronized
    /// subsurface keeps it until the parent commits.
    fn commit(&mut self, id: u32) -> Result<Action, Broken> {
        if self.synchronized(id) {
            let surface = self.surfaces.get_mut(&id).ok_or(Broken::Protocol("no such surface"))?;
            let pending = core::mem::take(&mut surface.pending);
            match &mut surface.cached {
                Some(cached) => cached.merge(pending),
                None => surface.cached = Some(pending),
            }
            return Ok(Action::None);
        }
        let surface = self.surfaces.get_mut(&id).ok_or(Broken::Protocol("no such surface"))?;
        let pending = core::mem::take(&mut surface.pending);
        self.apply(id, pending);
        Ok(Action::Committed(id))
    }

    /// Whether a surface is a subsurface whose state waits for its parent
    /// (it or an ancestor is in synchronized mode).
    fn synchronized(&self, id: u32) -> bool {
        let mut current = id;
        for _ in 0..32 {
            match self.surfaces.get(&current).map(|surface| &surface.role) {
                Some(Role::Subsurface(sub)) if sub.sync => return true,
                Some(Role::Subsurface(sub)) => current = sub.parent,
                _ => return false,
            }
        }
        false
    }

    /// Applies committed state to a surface, then to its synchronized
    /// children's cached state.
    fn apply(&mut self, id: u32, pending: crate::surface::Pending) {
        let Some(surface) = self.surfaces.get_mut(&id) else { return };
        if let Some(scale) = pending.scale {
            surface.scale = scale;
        }
        let scale = surface.scale.max(1);
        // What changed, in surface coordinates (`None`: all of it).
        let mut changed = Some(pending.damage.unwrap_or(Rect::new(0, 0, 0, 0)));
        match pending.buffer {
            Some(None) => {
                surface.has_content = false;
                surface.pixels = Vec::new();
                changed = None;
            }
            Some(Some(id)) => {
                if let Some(buffer) = self.buffers.get(&id) {
                    let same_size = surface.has_content
                        && surface.buffer_width == buffer.width
                        && surface.height * scale == buffer.height;
                    let damage = pending
                        .damage
                        .filter(|_| same_size)
                        .map(|d| Rect::new(d.x * scale, d.y * scale, d.width * scale, d.height * scale));
                    if let Some(copied) = buffer.copy_into(&mut surface.pixels, damage) {
                        surface.buffer_width = buffer.width;
                        surface.width = buffer.width / scale;
                        surface.height = buffer.height / scale;
                        surface.alpha = buffer.format == FORMAT_ARGB8888;
                        surface.has_content = true;
                        changed = (copied != Rect::new(0, 0, buffer.width, buffer.height)).then(|| {
                            Rect::new(
                                copied.x / scale,
                                copied.y / scale,
                                copied.width / scale + 1,
                                copied.height / scale + 1,
                            )
                        });
                        // Copied: the client can have the buffer back.
                        self.send(Outgoing::new(id, 0));
                    }
                }
            }
            None => {}
        }
        self.applied.push((id, changed));
        let Some(surface) = self.surfaces.get_mut(&id) else { return };
        if let Some(region) = pending.input_region {
            surface.input_region = region;
        }
        surface.frame_callbacks.extend(pending.frame_callbacks);
        if let Some(xdg) = &mut surface.xdg {
            xdg.initial_commit = true;
        }
        // Children: positions set since the parent's last commit, and the
        // state of synchronized ones.
        let children = surface.children.clone();
        for child in children {
            let Some(child_surface) = self.surfaces.get_mut(&child) else { continue };
            if let Role::Subsurface(sub) = &mut child_surface.role
                && let Some((x, y)) = sub.pending_position.take()
                && (sub.x, sub.y) != (x, y)
            {
                sub.x = x;
                sub.y = y;
                self.moved = true;
            }
            if let Some(cached) = child_surface.cached.take() {
                self.apply(child, cached);
            }
        }
    }

    fn subsurface_request(
        &mut self,
        id: u32,
        surface: u32,
        opcode: u16,
        args: &mut Arguments,
    ) -> Result<Action, Broken> {
        let parent = match self.surfaces.get(&surface).map(|s| &s.role) {
            Some(Role::Subsurface(sub)) => sub.parent,
            _ => 0,
        };
        match opcode {
            0 => {
                if let Some(target) = self.surfaces.get_mut(&surface) {
                    target.role = Role::None;
                }
                if let Some(parent) = self.surfaces.get_mut(&parent) {
                    parent.children.retain(|&child| child != surface);
                }
                self.remove(id);
                return Ok(Action::Committed(parent));
            }
            1 => {
                let position = (args.int()?, args.int()?);
                if let Some(Role::Subsurface(sub)) = self.surfaces.get_mut(&surface).map(|s| &mut s.role) {
                    sub.pending_position = Some(position);
                }
            }
            // place_above / place_below a sibling (or the parent).
            2 | 3 => {
                let sibling = args.uint()?;
                if let Some(parent) = self.surfaces.get_mut(&parent) {
                    parent.children.retain(|&child| child != surface);
                    let index = parent.children.iter().position(|&child| child == sibling);
                    match (opcode, index) {
                        (2, Some(index)) => parent.children.insert(index + 1, surface),
                        (3, Some(index)) => parent.children.insert(index, surface),
                        // Relative to the parent itself: first or last.
                        (2, None) => parent.children.insert(0, surface),
                        _ => parent.children.insert(0, surface),
                    }
                }
            }
            4 | 5 => {
                let sync = opcode == 4;
                if let Some(Role::Subsurface(sub)) = self.surfaces.get_mut(&surface).map(|s| &mut s.role) {
                    sub.sync = sync;
                }
                // Going desynchronized applies what was cached.
                if !sync
                    && !self.synchronized(surface)
                    && let Some(cached) = self.surfaces.get_mut(&surface).and_then(|s| s.cached.take())
                {
                    self.apply(surface, cached);
                    return Ok(Action::Committed(surface));
                }
            }
            _ => {}
        }
        Ok(Action::None)
    }

    fn xdg_surface_request(
        &mut self,
        id: u32,
        surface: u32,
        opcode: u16,
        args: &mut Arguments,
        version: u32,
        serial: &mut u32,
    ) -> Result<Action, Broken> {
        match opcode {
            0 => {
                if let Some(target) = self.surfaces.get_mut(&surface) {
                    target.xdg = None;
                }
                self.remove(id);
            }
            // get_toplevel(id)
            1 => {
                let new = args.uint()?;
                let target = self.surfaces.get_mut(&surface).ok_or(Broken::Protocol("no such surface"))?;
                if !matches!(target.role, Role::None) {
                    return Err(Broken::Protocol("surface already has a role"));
                }
                target.role = Role::Toplevel(Toplevel {
                    object: new,
                    title: String::new(),
                    app_id: String::new(),
                    parent: None,
                    min_size: (0, 0),
                    max_size: (0, 0),
                    fullscreen: false,
                    x: 0,
                    y: 0,
                    configured: (0, 0),
                    activated: false,
                });
                self.add(new, Kind::Toplevel(surface), version)?;
                return Ok(Action::NewToplevel(surface));
            }
            // get_popup(id, parent xdg_surface, positioner)
            2 => {
                let (new, parent, positioner) = (args.uint()?, args.uint()?, args.uint()?);
                let parent_surface = match self.objects.get(&parent).map(|o| o.kind) {
                    Some(Kind::XdgSurface(parent_surface)) => parent_surface,
                    _ => return Err(Broken::Protocol("popups need a parent")),
                };
                let rect = self.place_popup(parent_surface, positioner);
                let target = self.surfaces.get_mut(&surface).ok_or(Broken::Protocol("no such surface"))?;
                target.role = Role::Popup(Popup { object: new, parent: parent_surface, rect, grab: false });
                self.add(new, Kind::Popup(surface), version)?;
                *serial += 1;
                return Ok(Action::NewPopup(surface));
            }
            3 => {
                let geometry = Rect::new(args.int()?, args.int()?, args.int()?, args.int()?);
                if let Some(xdg) = self.surfaces.get_mut(&surface).and_then(|s| s.xdg.as_mut()) {
                    xdg.geometry = Some(geometry);
                }
            }
            // ack_configure
            4 => {
                let _ = args.uint()?;
                if let Some(xdg) = self.surfaces.get_mut(&surface).and_then(|s| s.xdg.as_mut()) {
                    xdg.configured = true;
                }
            }
            _ => {}
        }
        Ok(Action::None)
    }

    /// Where a popup goes, relative to its parent's window geometry, from
    /// its positioner (before keeping it on screen, which the shell does).
    fn place_popup(&self, _parent: u32, positioner: u32) -> Rect {
        let Some(p) = self.positioners.get(&positioner) else { return Rect::new(0, 0, 1, 1) };
        let anchor = p.anchor_rect;
        // Anchor point: 0 none (center), 1 top, 2 bottom, 3 left, 4 right,
        // 5 top left, 6 bottom left, 7 top right, 8 bottom right.
        let ax = match p.anchor {
            3 | 5 | 6 => anchor.x,
            4 | 7 | 8 => anchor.x + anchor.width,
            _ => anchor.x + anchor.width / 2,
        };
        let ay = match p.anchor {
            1 | 5 | 7 => anchor.y,
            2 | 6 | 8 => anchor.y + anchor.height,
            _ => anchor.y + anchor.height / 2,
        };
        // Gravity: the direction the popup extends from the anchor point.
        let (width, height) = p.size;
        let x = match p.gravity {
            3 | 5 | 6 => ax - width,
            4 | 7 | 8 => ax,
            _ => ax - width / 2,
        };
        let y = match p.gravity {
            1 | 5 | 7 => ay - height,
            2 | 6 | 8 => ay,
            _ => ay - height / 2,
        };
        Rect::new(x + p.offset.0, y + p.offset.1, width, height)
    }

    fn toplevel_request(&mut self, id: u32, surface: u32, opcode: u16, args: &mut Arguments) -> Result<Action, Broken> {
        let Some(target) = self.surfaces.get_mut(&surface) else { return Ok(Action::None) };
        if opcode == 0 {
            target.role = Role::None;
            self.remove(id);
            return Ok(Action::SurfaceDestroyed(surface));
        }
        // Parents are toplevel objects; their surfaces are needed.
        let parent_object = if opcode == 1 { Some(args.uint()?) } else { None };
        let parent_surface = parent_object.and_then(|object| match self.objects.get(&object).map(|o| o.kind) {
            Some(Kind::Toplevel(parent)) => Some(parent),
            _ => None,
        });
        let Some(Role::Toplevel(toplevel)) = self.surfaces.get_mut(&surface).map(|s| &mut s.role) else {
            return Ok(Action::None);
        };
        match opcode {
            1 => toplevel.parent = parent_surface,
            2 => toplevel.title = args.string()?.unwrap_or_default(),
            3 => toplevel.app_id = args.string()?.unwrap_or_default(),
            7 => toplevel.max_size = (args.int()?, args.int()?),
            8 => toplevel.min_size = (args.int()?, args.int()?),
            11 => toplevel.fullscreen = true,
            12 => toplevel.fullscreen = false,
            // move, resize, maximize, minimize, window menu: every window
            // already fills the screen.
            _ => return Ok(Action::None),
        }
        Ok(Action::ToplevelChanged(surface))
    }
}
