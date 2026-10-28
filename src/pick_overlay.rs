//! Native Wayland picker. Surface coordinates are already logical pixels;
//! buffer scale must never be applied to the captured point.
use std::fs::File;
use std::io::{BufWriter, Write};
use std::os::fd::{AsFd, FromRawFd};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};

#[derive(Default)]
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    shell: Option<xdg_wm_base::XdgWmBase>,
    manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    outputs: Vec<Output>,
    focus: Option<usize>,
    local: (f64, f64),
    result: Option<Option<(i32, i32)>>,
    cursor: Option<wl_surface::WlSurface>,
}
#[derive(Default)]
struct Output {
    output: Option<wl_output::WlOutput>,
    origin: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
    surface: Option<wl_surface::WlSurface>,
    configured: (i32, i32),
}

fn map_point(
    origin: (i32, i32),
    size: (i32, i32),
    surface: (i32, i32),
    local: (f64, f64),
) -> (i32, i32) {
    let axis = |o: i32, n: i32, s: i32, v: f64| {
        (i64::from(o)
            + (v * f64::from(n) / f64::from(s.max(1)))
                .floor()
                .clamp(0.0, f64::from(n - 1)) as i64) as i32
    };
    (
        axis(origin.0, size.0, surface.0, local.0),
        axis(origin.1, size.1, surface.1, local.1),
    )
}

pub fn pick() -> Result<Option<(i32, i32)>, String> {
    let run = || -> Result<_, Box<dyn std::error::Error>> {
        let conn = Connection::connect_to_env()?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let _registry = conn.display().get_registry(&qh, ());
        let mut state = State::default();
        queue.roundtrip(&mut state)?;
        let manager = state
            .manager
            .as_ref()
            .ok_or("Wayland picker requires xdg-output")?;
        for (i, output) in state.outputs.iter().enumerate() {
            manager.get_xdg_output(output.output.as_ref().unwrap(), &qh, i);
        }
        queue.roundtrip(&mut state)?;
        queue.roundtrip(&mut state)?;
        let compositor = state
            .compositor
            .as_ref()
            .ok_or("missing Wayland compositor")?;
        let shell = state.shell.as_ref().ok_or("missing xdg-shell")?;
        let shm = state.shm.as_ref().ok_or("missing shared memory")?;
        let cursor = compositor.create_surface(&qh, ());
        let pixels: Vec<u32> = (0..25 * 25)
            .map(|i| {
                if i % 25 == 12 || i / 25 == 12 {
                    0xffffffff
                } else {
                    0
                }
            })
            .collect();
        attach(shm, &cursor, 25, 25, &pixels, &qh)?;
        state.cursor = Some(cursor);
        if state.outputs.is_empty() {
            return Err("no Wayland outputs".into());
        }
        for (i, output) in state.outputs.iter_mut().enumerate() {
            if output.origin.is_none() || output.size.is_none() {
                return Err("output has no logical geometry".into());
            }
            let surface = compositor.create_surface(&qh, ());
            let xdg = shell.get_xdg_surface(&surface, &qh, i);
            let top = xdg.get_toplevel(&qh, i);
            top.set_title("FeatherClick".into());
            top.set_app_id("featherclick".into());
            top.set_fullscreen(output.output.as_ref());
            surface.commit();
            output.surface = Some(surface);
        }
        while state.result.is_none() {
            queue.blocking_dispatch(&mut state)?;
        }
        Ok(state.result.unwrap())
    };
    run().map_err(|err| format!("cannot pick a Wayland point: {err}"))
}

fn attach(
    shm: &wl_shm::WlShm,
    surface: &wl_surface::WlSurface,
    w: i32,
    h: i32,
    pixels: &[u32],
    qh: &QueueHandle<State>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Anonymous memory avoids paths, cleanup races, and a new dependency.
    let fd = unsafe { libc::memfd_create(c"featherclick-pick".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let mut writer = BufWriter::new(&file);
    for pixel in pixels {
        writer.write_all(&pixel.to_ne_bytes())?;
    }
    writer.flush()?;
    let pool = shm.create_pool(file.as_fd(), w * h * 4, qh, ());
    let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, qh, ());
    surface.attach(Some(&buffer), 0, 0);
    surface.damage(0, 0, w, h);
    surface.commit();
    pool.destroy();
    Ok(())
}
impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        s: &mut Self,
        r: &wl_registry::WlRegistry,
        e: wl_registry::Event,
        _: &(),
        _: &Connection,
        q: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = e
        {
            match interface.as_str() {
                "wl_compositor" => s.compositor = Some(r.bind(name, version.min(4), q, ())),
                "wl_shm" => s.shm = Some(r.bind(name, 1, q, ())),
                "xdg_wm_base" => s.shell = Some(r.bind(name, 1, q, ())),
                "zxdg_output_manager_v1" => s.manager = Some(r.bind(name, version.min(3), q, ())),
                "wl_output" => {
                    let output = r.bind(name, version.min(3), q, ());
                    s.outputs.push(Output {
                        output: Some(output),
                        ..Default::default()
                    });
                }
                "wl_seat" => {
                    let _: wl_seat::WlSeat = r.bind(name, version.min(5), q, ());
                }
                _ => {}
            }
        }
    }
}
impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        seat: &wl_seat::WlSeat,
        e: wl_seat::Event,
        _: &(),
        _: &Connection,
        q: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(c),
        } = e
        {
            if c.contains(wl_seat::Capability::Pointer) {
                seat.get_pointer(q, ());
            }
            if c.contains(wl_seat::Capability::Keyboard) {
                seat.get_keyboard(q, ());
            }
        }
    }
}
impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        s: &mut Self,
        p: &wl_pointer::WlPointer,
        e: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } => {
                s.focus = s
                    .outputs
                    .iter()
                    .position(|o| o.surface.as_ref() == Some(&surface));
                s.local = (surface_x, surface_y);
                p.set_cursor(serial, s.cursor.as_ref(), 12, 12);
            }
            wl_pointer::Event::Leave { .. } => s.focus = None,
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => s.local = (surface_x, surface_y),
            wl_pointer::Event::Button {
                state: WEnum::Value(wl_pointer::ButtonState::Pressed),
                ..
            } => {
                if let Some(i) = s.focus {
                    let o = &s.outputs[i];
                    s.result = Some(Some(map_point(
                        o.origin.unwrap(),
                        o.size.unwrap(),
                        o.configured,
                        s.local,
                    )));
                }
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        s: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        e: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Key {
            key: 1,
            state: WEnum::Value(wl_keyboard::KeyState::Pressed),
            ..
        } = e
        {
            s.result = Some(None);
        }
    }
}
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        shell: &xdg_wm_base::XdgWmBase,
        e: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = e {
            shell.pong(serial);
        }
    }
}
impl Dispatch<zxdg_output_v1::ZxdgOutputV1, usize> for State {
    fn event(
        s: &mut Self,
        _: &zxdg_output_v1::ZxdgOutputV1,
        e: zxdg_output_v1::Event,
        i: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            zxdg_output_v1::Event::LogicalPosition { x, y } => s.outputs[*i].origin = Some((x, y)),
            zxdg_output_v1::Event::LogicalSize { width, height } if width > 0 && height > 0 => {
                s.outputs[*i].size = Some((width, height))
            }
            _ => {}
        }
    }
}
impl Dispatch<xdg_toplevel::XdgToplevel, usize> for State {
    fn event(
        s: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        e: xdg_toplevel::Event,
        i: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                s.outputs[*i].configured = if width > 0 && height > 0 {
                    (width, height)
                } else {
                    s.outputs[*i].size.unwrap()
                }
            }
            xdg_toplevel::Event::Close => s.result = Some(None),
            _ => {}
        }
    }
}
impl Dispatch<xdg_surface::XdgSurface, usize> for State {
    fn event(
        s: &mut Self,
        x: &xdg_surface::XdgSurface,
        e: xdg_surface::Event,
        i: &usize,
        _: &Connection,
        q: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = e {
            x.ack_configure(serial);
            let o = &s.outputs[*i];
            let (w, h) = o.configured;
            if w <= 0 || h <= 0 || i64::from(w) * i64::from(h) > 32 * 1024 * 1024 {
                s.result = Some(None);
                return;
            }
            if attach(
                s.shm.as_ref().unwrap(),
                o.surface.as_ref().unwrap(),
                w,
                h,
                &vec![0x22000000; w as usize * h as usize],
                q,
            )
            .is_err()
            {
                s.result = Some(None);
            }
        }
    }
}
delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn event(
        _: &mut Self,
        buffer: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            buffer.destroy();
        }
    }
}
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_output::WlOutput);
delegate_noop!(State: ignore zxdg_output_manager_v1::ZxdgOutputManagerV1);

#[cfg(test)]
mod tests {
    use super::map_point;
    #[test]
    fn logical_coordinates_ignore_fractional_buffer_scale() {
        assert_eq!(
            map_point((-1536, -200), (1536, 864), (1536, 864), (100.75, 200.5)),
            (-1436, 0)
        );
    }
    #[test]
    fn configured_extent_maps_and_clamps_to_output() {
        assert_eq!(
            map_point((-1920, 0), (1920, 1080), (960, 540), (480.5, 270.5)),
            (-959, 541)
        );
        assert_eq!(
            map_point((100, -50), (800, 600), (800, 600), (800.0, -1.0)),
            (899, -50)
        );
    }
}
