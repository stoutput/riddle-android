//! A host-side harness for looking at what the engine actually draws.
//!
//! The Android UI can only be inspected through screenshots, which makes
//! "is the page blank because nothing was drawn, or because the frame never
//! reached the view?" hard to answer. This dumps the engine's own pixel buffer
//! to a PNG after a scripted sequence, so the drawing can be checked — and
//! diffed — without a device.
//!
//! Usage:
//!   cargo run --example dump_page -- guide   # the opening guide panel
//!   cargo run --example dump_page -- reply   # a synthesised reply
//!   cargo run --example dump_page -- ink     # a hand-drawn stroke
//!
//! Output goes to `dumped-<what>.png` in the current directory.

use riddle::app::{App, Host, PenSample, Tool};

/// The harness has no Android to talk to; it just remembers what it was told.
struct TestHost {
    logs: std::cell::RefCell<Vec<String>>,
}

impl Host for TestHost {
    fn on_open_settings(&self) {}
    fn request_repaint(&self) {}
    fn log(&self, msg: &str) {
        self.logs.borrow_mut().push(msg.to_string());
    }
}

fn write_png(surf_pixels: &[u16], w: u32, h: u32, path: &str) -> std::io::Result<()> {
    // Surface pixels are little-endian RGB565; expand to 8-bit gray so the
    // output matches how the page is meant to look (mono ink on white).
    let mut gray = vec![0u8; (w * h) as usize];
    for (i, &v) in surf_pixels.iter().enumerate() {
        let v = u16::from_le(v);
        let g = ((v >> 5) & 0x3f) as u32;
        gray[i] = (g * 255 / 63) as u8;
    }
    let file = std::fs::File::create(path)?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Grayscale);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(std::io::Error::other)?;
    writer.write_image_data(&gray).map_err(std::io::Error::other)?;
    Ok(())
}

fn main() -> std::io::Result<()> {
    let what = std::env::args().nth(1).unwrap_or_else(|| "guide".to_string());
    let host = TestHost { logs: std::cell::RefCell::new(Vec::new()) };

    // The engine owns its page buffer, so the harness reads the pixels back
    // out of it rather than supplying its own surface.
    let mut app = match App::new(&host) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("App::new failed: {e}");
            std::process::exit(1);
        }
    };
    app.start();

    match what.as_str() {
        "guide" => {
            // App::new already opened the guide when no oracle is configured;
            // if a key happens to be present, ask for it explicitly.
            app.open_guide();
        }
        "reply" => {
            app.write_line("Yes, Harry? I have been waiting for you.");
        }
        "ink" => {
            // A short diagonal stroke, as a pen would deliver it.
            for (i, y) in (400..1200).enumerate() {
                app.push_input(PenSample {
                    x: 300 + i as i32,
                    y,
                    pressure: 1200,
                    tool: Tool::Pen,
                    touching: true,
                });
            }
            app.pen_up();
        }
        other => {
            eprintln!("unknown target: {other}");
            std::process::exit(2);
        }
    }

    // Advance the animation well past any scheduled work.
    for _ in 0..400 {
        app.step(&host);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }

    let px = app.pixels();
    let w = 1620u32;
    let h = 2160u32;
    let path = format!("dumped-{what}.png");
    write_png(px, w, h, &path)?;
    println!("wrote {path}");
    println!("state: {}", app.state_name());
    println!("{}", app.describe_page());
    println!("--- engine log ---");
    for line in host.logs.borrow().iter() {
        println!("{line}");
    }
    Ok(())
}
