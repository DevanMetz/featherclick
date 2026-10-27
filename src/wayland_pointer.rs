//! wlroots virtual pointer, aimed in the compositor's layout.
//!
//! The pointer is not bound to one output. wlroots then maps absolute motion
//! across the bounding box of every output, which is the same space
//! `xdg-output` reports. Binding it to a single output would only be right on
//! compositors that honour that hint.

use std::time::Instant;

use wayland_client::protocol::wl_output::{self, Mode, Transform, WlOutput};
use wayland_client::protocol::wl_pointer::ButtonState;
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1;
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::{self, ZxdgOutputV1};
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;

use crate::pointer_layout::{self, AbsoluteMotion, ReportedOutput};

pub struct WaylandPointer {
    queue: EventQueue<State>,
    state: State,
    started: Instant,
}

struct State {
    // Kept alive: dropping the proxy destroys the registry and stops global events.
    _registry: Option<wl_registry::WlRegistry>,
    seat_name: Option<u32>,
    seat: Option<WlSeat>,
    pointer_manager_name: Option<u32>,
    pointer_manager: Option<ZwlrVirtualPointerManagerV1>,
    pointer: Option<ZwlrVirtualPointerV1>,
    output_manager_name: Option<u32>,
    output_manager: Option<ZxdgOutputManagerV1>,
    outputs: Vec<Tracked>,
}

struct Tracked {
    name: u32,
    output: WlOutput,
    xdg: Option<ZxdgOutputV1>,
    logical_position: Option<(i32, i32)>,
    logical_size: Option<(i32, i32)>,
    mode_size: Option<(i32, i32)>,
    scale: i32,
    transform_swaps_axes: bool,
}

impl WaylandPointer {
    pub fn connect() -> Result<Self, String> {
        let connection = Connection::connect_to_env()
            .map_err(|_| "cannot connect to the wayland display".to_string())?;
        let queue = connection.new_event_queue();
        let qh = queue.handle();
        let registry = connection.display().get_registry(&qh, ());
        let mut pointer = Self {
            queue,
            state: State {
                _registry: Some(registry),
                seat_name: None,
                seat: None,
                pointer_manager_name: None,
                pointer_manager: None,
                pointer: None,
                output_manager_name: None,
                output_manager: None,
                outputs: Vec::new(),
            },
            started: Instant::now(),
        };
        // Globals, then the xdg-output objects those globals create, then the
        // logical geometry. Three trips is enough for a compositor that sends
        // `done` on its own round.
        pointer.roundtrip()?;
        pointer.attach();
        pointer.roundtrip()?;
        pointer.roundtrip()?;
        if pointer.state.pointer.is_none() {
            return Err("this compositor has no virtual pointer".into());
        }
        Ok(pointer)
    }

    pub fn move_abs(&mut self, x: i32, y: i32) -> Result<(), String> {
        // The usual path sends one motion. A miss can be a monitor that
        // appeared after connect: bind it, ask for its logical geometry, and
        // send the motion once.
        if self.try_move(x, y).is_ok() {
            return Ok(());
        }
        self.roundtrip()?;
        self.attach();
        self.roundtrip()?;
        self.try_move(x, y)
    }

    pub fn button(&mut self, code: u32, pressed: bool) -> Result<(), String> {
        let time = self.millis();
        let state = if pressed {
            ButtonState::Pressed
        } else {
            ButtonState::Released
        };
        {
            let pointer = self.pointer()?;
            pointer.button(time, code, state);
            pointer.frame();
        }
        self.flush()
    }

    fn try_move(&mut self, x: i32, y: i32) -> Result<(), String> {
        let motion = self.motion(x, y)?;
        let time = self.millis();
        {
            let pointer = self.pointer()?;
            pointer.motion_absolute(time, motion.x, motion.y, motion.x_extent, motion.y_extent);
            pointer.frame();
        }
        self.flush()
    }

    fn motion(&self, x: i32, y: i32) -> Result<AbsoluteMotion, String> {
        let reported: Vec<ReportedOutput> =
            self.state.outputs.iter().map(Tracked::reported).collect();
        let outputs = pointer_layout::layout_from(&reported).map_err(|err| err.message())?;
        pointer_layout::absolute_motion(&outputs, x, y).map_err(|err| err.message())
    }

    fn pointer(&self) -> Result<&ZwlrVirtualPointerV1, String> {
        self.state
            .pointer
            .as_ref()
            .ok_or_else(|| "this compositor has no virtual pointer".to_string())
    }

    fn attach(&mut self) {
        let qh = self.queue.handle();
        // After the registry round trip, so the seat is known and the pointer
        // is assigned to it. Creating it from the global event would miss the
        // seat when the manager is advertised first.
        ensure_pointer(&mut self.state, &qh);
        if let Some(manager) = self.state.output_manager.clone() {
            for output in &mut self.state.outputs {
                if output.xdg.is_none() {
                    output.xdg = Some(manager.get_xdg_output(&output.output, &qh, ()));
                }
            }
        }
    }

    fn roundtrip(&mut self) -> Result<(), String> {
        self.queue
            .roundtrip(&mut self.state)
            .map(|_| ())
            .map_err(|err| err.to_string())
    }

    fn flush(&self) -> Result<(), String> {
        self.queue.flush().map_err(|err| err.to_string())
    }

    fn millis(&self) -> u32 {
        u32::try_from(self.started.elapsed().as_millis()).unwrap_or(u32::MAX)
    }
}

impl Tracked {
    fn reported(&self) -> ReportedOutput {
        ReportedOutput {
            logical_position: self.logical_position,
            logical_size: self.logical_size,
            mode_size: self.mode_size,
            scale: self.scale,
            transform_swaps_axes: self.transform_swaps_axes,
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        (): &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => bind_global(state, registry, qh, name, &interface, version),
            wl_registry::Event::GlobalRemove { name } => {
                state.outputs.retain(|output| output.name != name);
                if state.seat_name == Some(name) {
                    state.seat = None;
                    state.seat_name = None;
                }
                if state.pointer_manager_name == Some(name) {
                    state.pointer_manager = None;
                    state.pointer = None;
                    state.pointer_manager_name = None;
                }
                if state.output_manager_name == Some(name) {
                    state.output_manager = None;
                    state.output_manager_name = None;
                }
            }
            _ => {}
        }
    }
}

fn bind_global(
    state: &mut State,
    registry: &wl_registry::WlRegistry,
    qh: &QueueHandle<State>,
    name: u32,
    interface: &str,
    version: u32,
) {
    if version == 0 {
        return;
    }
    match interface {
        "wl_seat" if state.seat.is_none() => {
            state.seat = Some(registry.bind(name, version.min(7), qh, ()));
            state.seat_name = Some(name);
        }
        "wl_output" => {
            state.outputs.push(Tracked {
                name,
                output: registry.bind(name, version.min(4), qh, ()),
                xdg: None,
                logical_position: None,
                logical_size: None,
                mode_size: None,
                scale: 1,
                transform_swaps_axes: false,
            });
        }
        "zwlr_virtual_pointer_manager_v1" if state.pointer_manager.is_none() => {
            state.pointer_manager = Some(registry.bind(name, version.min(2), qh, ()));
            state.pointer_manager_name = Some(name);
        }
        "zxdg_output_manager_v1" if state.output_manager.is_none() => {
            state.output_manager = Some(registry.bind(name, version.min(3), qh, ()));
            state.output_manager_name = Some(name);
        }
        _ => {}
    }
}

fn ensure_pointer(state: &mut State, qh: &QueueHandle<State>) {
    if state.pointer.is_some() {
        return;
    }
    let Some(manager) = state.pointer_manager.clone() else {
        return;
    };
    state.pointer = Some(manager.create_virtual_pointer(state.seat.as_ref(), qh, ()));
}

fn swaps_axes(transform: WEnum<Transform>) -> bool {
    matches!(
        transform,
        WEnum::Value(
            Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270
        )
    )
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: <WlSeat as Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: wl_output::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(tracked) = state
            .outputs
            .iter_mut()
            .find(|tracked| &tracked.output == output)
        else {
            return;
        };
        match event {
            wl_output::Event::Geometry { transform, .. } => {
                tracked.transform_swaps_axes = swaps_axes(transform);
            }
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } => {
                let current = matches!(flags, WEnum::Value(bits) if bits.contains(Mode::Current));
                if current || tracked.mode_size.is_none() {
                    tracked.mode_size = Some((width, height));
                }
            }
            wl_output::Event::Scale { factor } => {
                tracked.scale = factor;
            }
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZxdgOutputManagerV1,
        _: <ZxdgOutputManagerV1 as Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZxdgOutputV1, ()> for State {
    fn event(
        state: &mut Self,
        xdg: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(tracked) = state
            .outputs
            .iter_mut()
            .find(|tracked| tracked.xdg.as_ref() == Some(xdg))
        else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                tracked.logical_position = Some((x, y));
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                tracked.logical_size = Some((width, height));
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerManagerV1,
        _: <ZwlrVirtualPointerManagerV1 as Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerV1,
        _: <ZwlrVirtualPointerV1 as Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
