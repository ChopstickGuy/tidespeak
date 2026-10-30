// Release builds on Windows: no console window
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

slint::include_modules!();

mod ui;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use slint::winit_030::{winit, WinitWindowAccessor};
use slint::{ComponentHandle, Image, Model, ModelRc, Rgb8Pixel, SharedPixelBuffer, VecModel};
use tidespeak::audio::Audio;
use tidespeak::config::Config;
use tidespeak::net::{self, Command};
use tidespeak::render::{Renderer, CELL_H, CELL_W};
use ui::{App, WinAct};

struct Ctx {
    app: App,
    renderer: Renderer,
    last: Instant,
    tiles: Rc<VecModel<Tile>>,
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn,tidespeak=info"))
        .format_target(false)
        .init();

    let cfg = Config::load();
    tidespeak::i18n::apply(&cfg.language);
    let audio = Arc::new(Audio::start(ui::audio_settings(&cfg)));
    let window = MainWindow::new().expect("Fenster");
    // Wayland/X11: passt zu tidespeak.desktop, damit GNOME Symbol und Fenster verbindet
    let _ = slint::set_xdg_app_id("tidespeak");

    let ctx: Rc<RefCell<Option<Ctx>>> = Rc::new(RefCell::new(None));
    // Ereignisse aus dem Netz-Thread landen in einer Warteschlange und werden im nächsten Takt verarbeitet
    let net_tx = net::spawn(audio.clone(), move |ev| {
        let _ = slint::invoke_from_event_loop(move || NET_EVENTS.with(|q| q.borrow_mut().push(ev)));
    });

    window.set_text0(cfg.address.clone().into());
    window.set_text1(cfg.name.clone().into());
    let scale = window.window().scale_factor();
    let tiles = Rc::new(VecModel::<Tile>::default());
    window.set_tiles(ModelRc::from(tiles.clone()));
    *ctx.borrow_mut() = Some(Ctx { app: App::new(cfg, audio.clone(), net_tx.clone()), renderer: Renderer::new(scale), last: Instant::now(), tiles });
    wire(&window, &ctx);

    // Zeittakt: Ereignisse verarbeiten, Animationen weiterschalten, bei Bedarf neu zeichnen
    let timer = slint::Timer::default();
    {
        let weak = window.as_weak();
        let ctx = ctx.clone();
        timer.start(slint::TimerMode::Repeated, Duration::from_millis(40), move || {
            let Some(w) = weak.upgrade() else { return };
            frame(&w, &ctx, false);
        });
    }
    frame(&window, &ctx, true);

    window.run().expect("Ereignisschleife");
    if let Some(c) = ctx.borrow().as_ref() {
        c.app.cfg.save();
    }
    let _ = net_tx.send(Command::Disconnect);
    std::thread::sleep(Duration::from_millis(100));
}

thread_local! {
    static NET_EVENTS: RefCell<Vec<net::Event>> = const { RefCell::new(Vec::new()) };
}

/// One step: network events, animation, drawing, window actions
fn frame(w: &MainWindow, ctx: &Rc<RefCell<Option<Ctx>>>, force: bool) {
    let mut guard = ctx.borrow_mut();
    let Some(c) = guard.as_mut() else { return };
    for ev in NET_EVENTS.with(|q| std::mem::take(&mut *q.borrow_mut())) {
        c.app.net_event(ev);
    }
    let now = Instant::now();
    let dt = now.duration_since(c.last).as_secs_f32().min(0.2);
    c.last = now;
    let redraw = c.app.tick(dt) || force;

    let win = w.window();
    let scale = win.scale_factor();
    let size = win.size();
    let (lw, lh) = (size.width as f32 / scale, size.height as f32 / scale);
    let cols = (lw / CELL_W) as i32;
    let rows = (lh / CELL_H) as i32;
    let maximized = win.is_maximized();
    if w.get_is_maximized() != maximized {
        w.set_is_maximized(maximized);
    }
    if redraw || cols != c.app.cols || rows != c.app.rows {
        c.app.draw(rows, cols);
        c.renderer.set_scale(scale);
        let changed = c.renderer.render(&c.app.grid);
        if changed || force {
            // Nur geänderte Kacheln übergeben – das Fenster zeichnet dann nur diese Bereiche neu
            let count = c.renderer.tiles_x() * c.renderer.tiles_y();
            if c.tiles.row_count() != count {
                c.tiles.set_vec(vec![Tile::default(); count]);
            }
            for t in c.renderer.take_tiles() {
                let buf = SharedPixelBuffer::<Rgb8Pixel>::clone_from_slice(&t.rgb, t.w, t.h);
                c.tiles.set_row_data(
                    t.index,
                    Tile { x: t.x as f32 / scale, y: t.y as f32 / scale, w: t.w as f32 / scale, h: t.h as f32 / scale, img: Image::from_rgb8(buf) },
                );
            }
        }
        for i in 0..3 {
            let (shown, x, y, fw) = c.app.field_px(i);
            match i {
                0 => {
                    w.set_shown0(shown);
                    w.set_x0(x);
                    w.set_y0(y);
                    w.set_w0(fw);
                }
                1 => {
                    w.set_shown1(shown);
                    w.set_x1(x);
                    w.set_y1(y);
                    w.set_w1(fw);
                }
                _ => {
                    w.set_shown2(shown);
                    w.set_x2(x);
                    w.set_y2(y);
                    w.set_w2(fw);
                }
            }
        }
        w.set_hand(c.app.hover.is_some());
    }
    let actions = std::mem::take(&mut c.app.win);
    drop(guard);
    for a in actions {
        match a {
            WinAct::Minimize => w.window().set_minimized(true),
            WinAct::Maximize => {
                let m = !w.window().is_maximized();
                w.window().set_maximized(m);
                w.set_is_maximized(m);
            }
            WinAct::Drag => {
                w.window().with_winit_window(|ww: &winit::window::Window| {
                    let _ = ww.drag_window();
                });
            }
            WinAct::Quit => {
                let _ = slint::quit_event_loop();
            }
            WinAct::Focus(i) => w.invoke_focus_field(i),
            WinAct::SetText(i, s) => match i {
                0 => w.set_text0(s.into()),
                1 => w.set_text1(s.into()),
                _ => w.set_text2(s.into()),
            },
            WinAct::Attention => {
                w.window().with_winit_window(|ww: &winit::window::Window| {
                    if !ww.has_focus() {
                        ww.request_user_attention(Some(winit::window::UserAttentionType::Informational));
                    }
                });
            }
        }
    }
}

fn with_app(ctx: &Rc<RefCell<Option<Ctx>>>, f: impl FnOnce(&mut App)) {
    if let Some(c) = ctx.borrow_mut().as_mut() {
        f(&mut c.app);
    }
}

fn wire(window: &MainWindow, ctx: &Rc<RefCell<Option<Ctx>>>) {
    {
        let ctx = ctx.clone();
        let weak = window.as_weak();
        window.on_mouse(move |kind, x, y| {
            with_app(&ctx, |a| a.pointer(kind.as_str(), x, y));
            if let Some(w) = weak.upgrade() {
                frame(&w, &ctx, false);
            }
        });
    }
    {
        let ctx = ctx.clone();
        let weak = window.as_weak();
        window.on_scrolled(move |x, y, dy| {
            with_app(&ctx, |a| a.scroll(x, y, dy));
            if let Some(w) = weak.upgrade() {
                frame(&w, &ctx, false);
            }
        });
    }
    {
        let ctx = ctx.clone();
        window.on_key(move |text, down| {
            let mut handled = false;
            with_app(&ctx, |a| handled = a.key(text.as_str(), down));
            handled
        });
    }
    {
        let ctx = ctx.clone();
        window.on_field_edited(move |i, text| with_app(&ctx, |a| a.field_edited(i as usize, text.to_string())));
    }
    {
        let ctx = ctx.clone();
        let weak = window.as_weak();
        window.on_field_accepted(move |i| {
            with_app(&ctx, |a| a.field_accepted(i as usize));
            if let Some(w) = weak.upgrade() {
                frame(&w, &ctx, false);
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_start_resize(move |dir| {
            use winit::window::ResizeDirection as D;
            let d = match dir {
                0 => D::North,
                1 => D::NorthEast,
                2 => D::East,
                3 => D::SouthEast,
                4 => D::South,
                5 => D::SouthWest,
                6 => D::West,
                _ => D::NorthWest,
            };
            if let Some(w) = weak.upgrade() {
                w.window().with_winit_window(|ww: &winit::window::Window| {
                    let _ = ww.drag_resize_window(d);
                });
            }
        });
    }
    {
        // Schließen über das System (Alt+F4, Taskleiste): erst die Abschiedssequenz
        let ctx = ctx.clone();
        window.window().on_close_requested(move || {
            let mut quit = false;
            with_app(&ctx, |a| {
                a.request_close();
                quit = a.win.iter().any(|w| matches!(w, WinAct::Quit));
            });
            if quit {
                slint::CloseRequestResponse::HideWindow
            } else {
                slint::CloseRequestResponse::KeepWindowShown
            }
        });
    }
}
